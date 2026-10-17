use std::fs;
use std::path::Path;

use serde_json::json;
use wasmtime::component::{Component, Linker};
use wasmtime::StoreContextMut;
use wasmtime::{Engine, Store};

use crate::host_state::HostState;
use crate::imports::{
    atomic_cas_write_locked, atomic_write_locked, crawl_cdp_value, exec_js_value, fetch_value,
    git_value, has_project_marker, kv_file_path, kv_namespace_dir, normalize_lexically,
    plugin_call_bytes, sandboxed_guest_path_with_extra_roots, task_proc_value, vec_embed_values,
};

const INTERFACE: &str = "plugkit:env/host@0.1.0";
const GUEST_INTERFACE: &str = "plugkit:env/guest@0.1.0";

fn sandboxed(state: &HostState, path: &str) -> Option<std::path::PathBuf> {
    let extra_roots = state.extra_readable_roots();
    sandboxed_guest_path_with_extra_roots(&state.cwd(), path, &extra_roots)
}

fn json_bytes(value: serde_json::Value) -> Option<Vec<u8>> {
    Some(value.to_string().into_bytes())
}

pub(crate) fn host_cwd(state: &HostState) -> Option<Vec<u8>> {
    Some(state.cwd().to_string_lossy().into_owned().into_bytes())
}

pub(crate) fn host_fs_allow_root(state: &HostState, path: &str) -> u32 {
    let Some(normalized) = normalize_lexically(Path::new(path)) else {
        return 0;
    };
    match fs::metadata(&normalized) {
        Ok(md) if md.is_dir() && has_project_marker(&normalized) => {
            state.allow_extra_root(normalized);
            1
        }
        _ => 0,
    }
}

pub(crate) fn host_fs_read(state: &HostState, path: &str) -> Option<Vec<u8>> {
    let full = sandboxed(state, path)?;
    fs::read(&full).ok()
}

pub(crate) fn host_fs_write(state: &HostState, path: &str, data: &[u8]) -> u32 {
    let Some(full) = sandboxed(state, path) else {
        return 0;
    };
    if let Some(parent) = full.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match atomic_write_locked(&full, &String::from_utf8_lossy(data)) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

pub(crate) fn host_fs_cas_write(state: &HostState, path: &str, expected: &[u8], data: &[u8]) -> u32 {
    let Some(full) = sandboxed(state, path) else {
        return 0;
    };
    if let Some(parent) = full.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match atomic_cas_write_locked(
        &full,
        &String::from_utf8_lossy(expected),
        &String::from_utf8_lossy(data),
    ) {
        Ok(true) => 1,
        Ok(false) => 2,
        Err(_) => 0,
    }
}

pub(crate) fn host_fs_remove(state: &HostState, path: &str) -> u32 {
    let Some(full) = sandboxed(state, path) else {
        return 0;
    };
    match fs::metadata(&full) {
        Ok(md) if md.is_dir() => 0,
        Ok(_) => match fs::remove_file(&full) {
            Ok(()) => 1,
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

pub(crate) fn host_fs_readdir(state: &HostState, path: &str) -> Option<Vec<u8>> {
    let full = sandboxed(state, path)?;
    let entries = fs::read_dir(&full)
        .and_then(|rd| {
            rd.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<String>>>()
        })
        .ok()?;
    json_bytes(json!(entries))
}

pub(crate) fn host_fs_stat(state: &HostState, path: &str) -> Option<Vec<u8>> {
    let full = sandboxed(state, path)?;
    let md = fs::metadata(&full).ok()?;
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let canonical_path = fs::canonicalize(&full)
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    json_bytes(json!({
        "isDirectory": md.is_dir(),
        "isFile": md.is_file(),
        "size": md.len(),
        "mtimeMs": mtime_ms,
        "mtime_ms": mtime_ms,
        "canonicalPath": canonical_path,
    }))
}

pub(crate) fn host_kv_get(state: &HostState, ns: &str, key: &str) -> Option<Vec<u8>> {
    if ns.is_empty() || key.is_empty() {
        return None;
    }
    fs::read(kv_file_path(&state.cwd(), ns, key)).ok()
}

pub(crate) fn host_kv_put(state: &HostState, ns: &str, key: &str, val: &[u8]) -> u32 {
    if ns.is_empty() || key.is_empty() {
        return 0;
    }
    let path = kv_file_path(&state.cwd(), ns, key);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(&path, val) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

pub(crate) fn host_kv_delete(state: &HostState, ns: &str, key: &str) -> u32 {
    if ns.is_empty() || key.is_empty() {
        return 0;
    }
    match fs::remove_file(kv_file_path(&state.cwd(), ns, key)) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

pub(crate) fn host_kv_query(state: &HostState, ns: &str, query: &str) -> Option<Vec<u8>> {
    if ns.is_empty() {
        return None;
    }
    let query = query.to_lowercase();
    let dir = kv_namespace_dir(&state.cwd(), ns);
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if query.is_empty() || content.to_lowercase().contains(&query) {
                    let key = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();
                    results.push(json!({"key": key, "value": content}));
                }
            }
        }
    }
    json_bytes(json!(results))
}

pub(crate) fn host_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn host_log(state: &HostState, level: u32, msg: &str) -> u32 {
    if let Some(evt_line) = msg.strip_prefix("evt: ") {
        crate::watcher_log::append_watcher_line(&state.cwd(), &format!("evt: {evt_line}"));
        return 1;
    }
    eprintln!("[agentplug:{} L{level}] {msg}", state.plugin_name);
    1
}

pub(crate) fn host_env_get(key: &str) -> Option<Vec<u8>> {
    if let Some(dispatch_value) = crate::dispatch_origin::dispatch_env_value(key) {
        return dispatch_value.map(String::into_bytes);
    }
    std::env::var(key).ok().map(String::into_bytes)
}

pub(crate) fn host_random_fill(len: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    getrandom::fill(&mut buf).ok()?;
    Some(buf)
}

pub(crate) fn host_vec_search(_query: &str, _k: u32) -> Option<Vec<u8>> {
    json_bytes(json!({"ok": false, "error": "host_vec_search_unused_guest_runs_libsql_directly"}))
}

pub(crate) fn register_component_env(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    let mut host = linker.instance(INTERFACE)?;
    host.func_wrap("host-cwd", |store: StoreContextMut<'_, HostState>, (): ()| {
        Ok((host_cwd(store.data()),))
    })?;
    host.func_wrap("host-fs-allow-root", |store: StoreContextMut<'_, HostState>, (path,): (String,)| {
        Ok((host_fs_allow_root(store.data(), &path),))
    })?;
    host.func_wrap("host-fs-read", |store: StoreContextMut<'_, HostState>, (path,): (String,)| {
        Ok((host_fs_read(store.data(), &path),))
    })?;
    host.func_wrap(
        "host-fs-write",
        |store: StoreContextMut<'_, HostState>, (path, data): (String, Vec<u8>)| {
            Ok((host_fs_write(store.data(), &path, &data),))
        },
    )?;
    host.func_wrap(
        "host-fs-cas-write",
        |store: StoreContextMut<'_, HostState>, (path, expected, data): (String, Vec<u8>, Vec<u8>)| {
            Ok((host_fs_cas_write(store.data(), &path, &expected, &data),))
        },
    )?;
    host.func_wrap("host-fs-remove", |store: StoreContextMut<'_, HostState>, (path,): (String,)| {
        Ok((host_fs_remove(store.data(), &path),))
    })?;
    host.func_wrap("host-fs-readdir", |store: StoreContextMut<'_, HostState>, (path,): (String,)| {
        Ok((host_fs_readdir(store.data(), &path),))
    })?;
    host.func_wrap("host-fs-stat", |store: StoreContextMut<'_, HostState>, (path,): (String,)| {
        Ok((host_fs_stat(store.data(), &path),))
    })?;
    host.func_wrap(
        "host-fetch",
        |_store: StoreContextMut<'_, HostState>, (url, opts): (String, String)| {
            Ok((json_bytes(fetch_value(&url, &opts)),))
        },
    )?;
    host.func_wrap(
        "host-kv-get",
        |store: StoreContextMut<'_, HostState>, (ns, key): (String, String)| {
            Ok((host_kv_get(store.data(), &ns, &key),))
        },
    )?;
    host.func_wrap(
        "host-kv-put",
        |store: StoreContextMut<'_, HostState>, (ns, key, val): (String, String, Vec<u8>)| {
            Ok((host_kv_put(store.data(), &ns, &key, &val),))
        },
    )?;
    host.func_wrap(
        "host-kv-delete",
        |store: StoreContextMut<'_, HostState>, (ns, key): (String, String)| {
            Ok((host_kv_delete(store.data(), &ns, &key),))
        },
    )?;
    host.func_wrap(
        "host-kv-query",
        |store: StoreContextMut<'_, HostState>, (ns, query): (String, String)| {
            Ok((host_kv_query(store.data(), &ns, &query),))
        },
    )?;
    host.func_wrap(
        "host-vec-search",
        |_store: StoreContextMut<'_, HostState>, (query, k): (String, u32)| {
            Ok((host_vec_search(&query, k),))
        },
    )?;
    host.func_wrap(
        "host-vec-embed",
        |store: StoreContextMut<'_, HostState>, (text, out_len): (String, u32)| {
            Ok((vec_embed_values(store.data(), &text, out_len),))
        },
    )?;
    host.func_wrap(
        "host-exec-js",
        |store: StoreContextMut<'_, HostState>, (code, opts): (String, String)| {
            Ok((json_bytes(exec_js_value(store.data(), &code, &opts)),))
        },
    )?;
    host.func_wrap(
        "host-log",
        |store: StoreContextMut<'_, HostState>, (level, msg): (u32, String)| {
            Ok((host_log(store.data(), level, &msg),))
        },
    )?;
    host.func_wrap("host-now-ms", |_store: StoreContextMut<'_, HostState>, (): ()| {
        Ok((host_now_ms(),))
    })?;
    host.func_wrap("host-env-get", |_store: StoreContextMut<'_, HostState>, (key,): (String,)| {
        Ok((host_env_get(&key),))
    })?;
    host.func_wrap("host-random-fill", |_store: StoreContextMut<'_, HostState>, (len,): (u32,)| {
        Ok((host_random_fill(len),))
    })?;
    host.func_wrap(
        "host-task-proc",
        |store: StoreContextMut<'_, HostState>, (action, params): (String, String)| {
            Ok((json_bytes(task_proc_value(store.data(), &action, &params)),))
        },
    )?;
    host.func_wrap(
        "host-git",
        |store: StoreContextMut<'_, HostState>, (args, cwd): (String, String)| {
            Ok((json_bytes(git_value(store.data(), &args, &cwd)),))
        },
    )?;
    host.func_wrap(
        "host-plugin-call",
        |store: StoreContextMut<'_, HostState>, (plugin, verb, body): (String, String, String)| {
            Ok((Some(plugin_call_bytes(store.data(), &plugin, &verb, &body)),))
        },
    )?;
    host.func_wrap("crawl-cdp", |store: StoreContextMut<'_, HostState>, (body,): (String,)| {
        Ok((json_bytes(crawl_cdp_value(store.data(), &body)),))
    })?;
    Ok(())
}

pub fn call_component_dispatch(
    engine: &Engine,
    component_bytes: &[u8],
    cwd: &Path,
    plugin_name: &str,
    verb: &str,
    body: &str,
) -> anyhow::Result<String> {
    let component = Component::new(engine, component_bytes)?;
    let mut linker: Linker<HostState> = Linker::new(engine);
    register_component_env(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        engine,
        HostState::new(cwd.to_path_buf(), plugin_name.to_string()),
    );
    store.set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(
        crate::registry::DISPATCH_CALL_DEADLINE_SECS,
    ));
    let instance = futures::executor::block_on(linker.instantiate_async(&mut store, &component))?;
    let guest = instance
        .get_export_index(&mut store, None, GUEST_INTERFACE)
        .ok_or_else(|| anyhow::anyhow!("component does not export {GUEST_INTERFACE}"))?;
    let dispatch_index = instance
        .get_export_index(&mut store, Some(&guest), "dispatch-verb")
        .ok_or_else(|| anyhow::anyhow!("{GUEST_INTERFACE} does not export dispatch-verb"))?;
    let dispatch = instance.get_typed_func::<(String, String), (String,)>(&mut store, dispatch_index)?;
    let (verb, body) = (verb.to_string(), body.to_string());
    let (reply,) = futures::executor::block_on(store.run_concurrent(async |accessor| {
        dispatch.call_concurrent(accessor, (verb, body)).await
    }))??;
    Ok(reply)
}

pub fn call_component_version(engine: &Engine, component_bytes: &[u8], cwd: &Path) -> anyhow::Result<String> {
    let component = Component::new(engine, component_bytes)?;
    let mut linker: Linker<HostState> = Linker::new(engine);
    register_component_env(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        engine,
        HostState::new(cwd.to_path_buf(), "plugkit".to_string()),
    );
    store.set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(
        crate::registry::DISPATCH_CALL_DEADLINE_SECS,
    ));
    let instance = futures::executor::block_on(linker.instantiate_async(&mut store, &component))?;
    let guest = instance
        .get_export_index(&mut store, None, GUEST_INTERFACE)
        .ok_or_else(|| anyhow::anyhow!("component does not export {GUEST_INTERFACE}"))?;
    let version_index = instance
        .get_export_index(&mut store, Some(&guest), "plugkit-version")
        .ok_or_else(|| anyhow::anyhow!("{GUEST_INTERFACE} does not export plugkit-version"))?;
    let version = instance.get_typed_func::<(), (String,)>(&mut store, version_index)?;
    let (reply,) = futures::executor::block_on(store.run_concurrent(async |accessor| {
        version.call_concurrent(accessor, ()).await
    }))??;
    Ok(reply)
}
