use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use wasmtime::{Engine, Module};

use crate::install::{legacy_wasmtime_cache_dir, precompiled_dir};

const PRECOMPILED_EXTENSION: &str = "cwasm";

fn remove_legacy_wasmtime_cache_once() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        let legacy = legacy_wasmtime_cache_dir();
        if !legacy.exists() {
            return;
        }
        match std::fs::remove_dir_all(&legacy) {
            Ok(()) => eprintln!(
                "[agentplug precompiled] removed the legacy wasmtime cache at {} (artifacts now live in {})",
                legacy.display(),
                precompiled_dir().display()
            ),
            Err(e) => eprintln!("[agentplug precompiled] could not remove the legacy wasmtime cache at {}: {e}", legacy.display()),
        }
    });
}

fn engine_compatibility_key(engine: &Engine) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn precompiled_file_name(plugin_name: &str, content_hash: &str, compat_key: &str) -> String {
    let short_hash: String = content_hash.chars().take(16).collect();
    format!("{plugin_name}-{short_hash}-{compat_key}.{PRECOMPILED_EXTENSION}")
}

pub fn precompiled_module_path(engine: &Engine, plugin_name: &str, content_hash: &str) -> PathBuf {
    precompiled_dir().join(precompiled_file_name(plugin_name, content_hash, &engine_compatibility_key(engine)))
}

fn is_hex_of_len(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_superseded_artifact_of(name: &str, plugin_name: &str, compat_key: &str) -> bool {
    let Some(rest) = name.strip_prefix(plugin_name).and_then(|r| r.strip_prefix('-')) else { return false };
    let Some(rest) = rest.strip_suffix(&format!(".{PRECOMPILED_EXTENSION}")) else { return false };
    let Some((content_part, key_part)) = rest.rsplit_once('-') else { return false };
    is_hex_of_len(content_part, 16) && key_part == compat_key
}

fn remove_superseded_artifacts_for(plugin_name: &str, compat_key: &str, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(precompiled_dir()) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_superseded_artifact_of(&name, plugin_name, compat_key) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn write_precompiled_artifact(engine: &Engine, wasm_path: &Path, artifact_path: &Path) -> anyhow::Result<()> {
    let wasm_bytes = std::fs::read(wasm_path)?;
    let serialized = engine.precompile_module(&wasm_bytes).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    drop(wasm_bytes);
    if let Some(parent) = artifact_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = artifact_path.with_extension(format!("{PRECOMPILED_EXTENSION}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &serialized)?;
    drop(serialized);
    std::fs::rename(&tmp, artifact_path)?;
    return_freed_compile_buffers_to_the_os();
    Ok(())
}

#[cfg(all(unix, target_env = "gnu"))]
fn return_freed_compile_buffers_to_the_os() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(unix, target_env = "gnu")))]
fn return_freed_compile_buffers_to_the_os() {}

fn deserialize_file_backed(engine: &Engine, artifact_path: &Path) -> anyhow::Result<Module> {
    unsafe { Module::deserialize_file(engine, artifact_path) }.map_err(|e| anyhow::anyhow!("{e:#}"))
}

pub fn load_module_file_backed(engine: &Engine, wasm_path: &Path, plugin_name: &str, content_hash: &str) -> anyhow::Result<Module> {
    remove_legacy_wasmtime_cache_once();
    let artifact_path = precompiled_module_path(engine, plugin_name, content_hash);
    if artifact_path.exists() {
        match deserialize_file_backed(engine, &artifact_path) {
            Ok(module) => return Ok(module),
            Err(e) => {
                eprintln!(
                    "[agentplug precompiled] {} failed to deserialize ({e:#}) -- removing it and recompiling from {}",
                    artifact_path.display(),
                    wasm_path.display()
                );
                let _ = std::fs::remove_file(&artifact_path);
            }
        }
    }
    let started = std::time::Instant::now();
    write_precompiled_artifact(engine, wasm_path, &artifact_path)?;
    remove_superseded_artifacts_for(plugin_name, &engine_compatibility_key(engine), &artifact_path);
    let artifact_len = std::fs::metadata(&artifact_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "[agentplug precompiled] {plugin_name} compiled to {} ({} MB, {}ms) -- later loads map this file instead of copying the artifact into anonymous memory",
        artifact_path.display(),
        artifact_len / (1024 * 1024),
        started.elapsed().as_millis()
    );
    deserialize_file_backed(engine, &artifact_path)
}
