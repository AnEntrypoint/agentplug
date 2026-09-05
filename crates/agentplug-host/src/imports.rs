use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use wait_timeout::ChildExt;
use wasmtime::{AsContextMut, Caller, Linker, Memory};

use crate::host_state::HostState;

fn fetch_agent() -> &'static ureq::Agent {
    crate::http_agent::shared_agent()
}

const GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE: u64 = 300_000;

#[derive(serde::Deserialize)]
struct CapabilityAllowlistConfig {
    #[serde(flatten)]
    allow: HashMap<String, Vec<String>>,
}

fn compiled_default_capability_allowlist(caller_plugin: &str, callee_plugin: &str) -> bool {
    match caller_plugin {
        "gm" => matches!(callee_plugin, "bert" | "libsql" | "treesitter" | "liqology" | "crux"),
        _ => false,
    }
}

fn load_capability_allowlist_config(cwd: &Path) -> Option<CapabilityAllowlistConfig> {
    let path = cwd.join(".agentplug").join("capability-allowlist.json");
    fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<CapabilityAllowlistConfig>(&s).ok())
}

fn plugin_call_capability_allowed(cwd: &Path, caller_plugin: &str, callee_plugin: &str) -> bool {
    match load_capability_allowlist_config(cwd) {
        Some(cfg) => cfg.allow.get(caller_plugin).map(|allowed| allowed.iter().any(|p| p == callee_plugin)).unwrap_or(false),
        None => compiled_default_capability_allowlist(caller_plugin, callee_plugin),
    }
}

fn is_well_formed_plugin_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_well_formed_verb(verb: &str) -> bool {
    !verb.is_empty() && verb.len() <= 128 && verb.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn validate_plugin_call_body(body: &str) -> Result<(), String> {
    if body.is_empty() {
        return Ok(());
    }
    if body.len() > 64 * 1024 * 1024 {
        return Err("body_exceeds_max_size".to_string());
    }
    serde_json::from_str::<serde_json::Value>(body).map(|_| ()).map_err(|e| format!("body_not_valid_json: {e}"))
}

pub fn git_subprocess_timeout_ms() -> u64 {
    std::env::var("AGENTPLUG_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE)
}

fn normalize_lexically(path: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn user_gm_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())?;
    let home = home.trim();
    if home.is_empty() {
        return None;
    }
    normalize_lexically(&std::path::Path::new(home).join(".gm"))
}

static FS_WRITE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();

fn fs_write_lock_for(path: &Path) -> std::sync::Arc<Mutex<()>> {
    let key = canonicalize_path_separators_for_stable_keying(path);
    let registry = FS_WRITE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.entry(key).or_insert_with(|| std::sync::Arc::new(Mutex::new(()))).clone()
}

fn atomic_write_locked(full: &Path, data: &str) -> std::io::Result<()> {
    let lock = fs_write_lock_for(full);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    atomic_write_under_held_lock(full, data)
}

fn atomic_write_under_held_lock(full: &Path, data: &str) -> std::io::Result<()> {
    let tmp = full.with_extension(format!(
        "{}.tmp-{}",
        full.extension().and_then(|e| e.to_str()).unwrap_or(""),
        std::process::id()
    ));
    fs::write(&tmp, data)?;
    match fs::rename(&tmp, full) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn atomic_cas_write_locked(full: &Path, expected: &str, data: &str) -> std::io::Result<bool> {
    let lock = fs_write_lock_for(full);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let current = fs::read_to_string(full).unwrap_or_default();
    if current != expected {
        return Ok(false);
    }
    atomic_write_under_held_lock(full, data)?;
    Ok(true)
}

fn has_project_marker(dir: &std::path::Path) -> bool {
    const MARKERS: &[&str] = &[
        ".git", ".gm", "package.json", "Cargo.toml", "go.mod", "pyproject.toml",
    ];
    MARKERS.iter().any(|m| dir.join(m).exists())
}

fn sandboxed_guest_path_with_extra_roots(cwd: &std::path::Path, path: &str, extra_roots: &[PathBuf]) -> Option<PathBuf> {
    let requested = std::path::Path::new(path);
    let joined = if requested.is_absolute() || requested.has_root() {
        requested.to_path_buf()
    } else {
        cwd.join(requested)
    };
    let normalized = normalize_lexically(&joined)?;
    let root = normalize_lexically(cwd)?;
    if normalized == root || normalized.starts_with(&root) {
        return Some(normalized);
    }
    let user_root = user_gm_root()?;
    if normalized.starts_with(&user_root) {
        return Some(normalized);
    }
    for extra in extra_roots {
        let Some(extra_normalized) = normalize_lexically(extra) else { continue };
        if normalized == extra_normalized || normalized.starts_with(&extra_normalized) {
            return Some(normalized);
        }
    }
    None
}

fn canonicalize_path_separators_for_stable_keying(path: &std::path::Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Memory {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("wasm module did not export linear memory")
}

fn read_guest_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    let memory = guest_memory(caller);
    let mut buf = vec![0u8; len as usize];
    let _ = memory.read(&mut *caller, ptr as usize, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

const PACKED_ABI_FIELD_MAX: usize = 0xffff_ffff;

fn pack_guest_ptr_len(ptr: u32, len: usize) -> u64 {
    debug_assert!(
        len <= PACKED_ABI_FIELD_MAX,
        "write_guest_bytes: length {len} exceeds the 32-bit field of the packed (ptr,len) ABI"
    );
    (ptr as u64 & 0xffff_ffff) | ((len as u64) << 32)
}

fn write_guest_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    if bytes.len() > PACKED_ABI_FIELD_MAX {
        let reason = format!(
            "host response of {} bytes exceeds the 32-bit packed-ABI field",
            bytes.len()
        );
        eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
        caller.data().note_lost_response(reason);
        return 0;
    }
    let instance = caller
        .data()
        .self_instance
        .lock()
        .unwrap()
        .expect("instance not yet bound to host state");
    let alloc = instance
        .get_typed_func::<u32, u32>(&mut *caller, "plugkit_alloc")
        .expect("plugkit_alloc export missing on wasm module");
    const RESPONSE_HANDOFF_GRACE_SECS: u64 = 5;
    let dispatch_deadline_secs_restored_after_handoff = caller.data().call_deadline_secs();
    caller.as_context_mut().set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(RESPONSE_HANDOFF_GRACE_SECS));
    match alloc.call(&mut *caller, bytes.len() as u32) {
        Ok(ptr) => {
            let memory = guest_memory(caller);
            if memory.write(&mut *caller, ptr as usize, bytes).is_err() {
                let reason = format!(
                    "guest memory.write of {} bytes at ptr {ptr} failed",
                    bytes.len()
                );
                eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
                caller.data().note_lost_response(reason);
                caller.as_context_mut().set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(dispatch_deadline_secs_restored_after_handoff));
                return 0;
            }
            caller.as_context_mut().set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(dispatch_deadline_secs_restored_after_handoff));
            pack_guest_ptr_len(ptr, bytes.len())
        }
        Err(e) => {
            let interrupted = matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::Interrupt));
            let reason = if interrupted {
                format!("plugkit_alloc({}) hit the epoch deadline while handing the host response back to the guest", bytes.len())
            } else {
                format!("plugkit_alloc({}) failed: {e}", bytes.len())
            };
            eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
            caller.data().note_lost_response(reason);
            caller.as_context_mut().set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(dispatch_deadline_secs_restored_after_handoff));
            0
        }
    }
}

fn write_guest_json(caller: &mut Caller<'_, HostState>, v: serde_json::Value) -> u64 {
    write_guest_bytes(caller, v.to_string().as_bytes())
}

pub fn register_wasi(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    wasmtime_wasi::p1::add_to_linker_sync(linker, |s: &mut HostState| &mut s.wasi)?;
    Ok(())
}

pub fn register_env_imports(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap(
        "env",
        "host_cwd",
        |mut caller: Caller<'_, HostState>| -> u64 {
            let cwd = caller.data().cwd().to_string_lossy().into_owned();
            write_guest_bytes(&mut caller, cwd.as_bytes())
        },
    )?;
    linker.func_wrap(
        "env",
        "host_fs_allow_root",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let Some(normalized) = normalize_lexically(std::path::Path::new(&path)) else { return 0 };
            match fs::metadata(&normalized) {
                Ok(md) if md.is_dir() && has_project_marker(&normalized) => {
                    caller.data().allow_extra_root(normalized);
                    1
                }
                _ => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_fs_read",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            match fs::read_to_string(&full) {
                Ok(content) => write_guest_bytes(&mut caller, content.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_write",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32, data_ptr: u32, data_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let data = read_guest_string(&mut caller, data_ptr, data_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match atomic_write_locked(&full, &data) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_cas_write",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32, expected_ptr: u32, expected_len: u32, data_ptr: u32, data_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let expected = read_guest_string(&mut caller, expected_ptr, expected_len);
            let data = read_guest_string(&mut caller, data_ptr, data_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match atomic_cas_write_locked(&full, &expected, &data) {
                Ok(true) => 1,
                Ok(false) => 2,
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_remove",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            match fs::metadata(&full) {
                Ok(md) if md.is_dir() => 0,
                Ok(_) => match fs::remove_file(&full) {
                    Ok(()) => 1,
                    Err(_) => 0,
                },
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_readdir",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            let entries: Vec<String> = fs::read_dir(&full)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            write_guest_json(&mut caller, serde_json::json!(entries))
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_stat",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            match fs::metadata(&full) {
                Ok(md) => {
                    let mtime_ms = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let v = serde_json::json!({"isDirectory": md.is_dir(), "isFile": md.is_file(), "size": md.len(), "mtimeMs": mtime_ms, "mtime_ms": mtime_ms});
                    write_guest_json(&mut caller, v)
                }
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap("env", "host_now_ms", |_caller: Caller<'_, HostState>| -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    })?;

    linker.func_wrap(
        "env",
        "host_log",
        |mut caller: Caller<'_, HostState>, level: u32, msg_ptr: u32, msg_len: u32| -> u32 {
            let msg = read_guest_string(&mut caller, msg_ptr, msg_len);
            let plugin = caller.data().plugin_name.clone();
            if let Some(evt_line) = msg.strip_prefix("evt: ") {
                let cwd = caller.data().cwd.lock().unwrap().clone();
                let log_path = cwd.join(".gm").join("exec-spool").join(".watcher.log");
                if let Some(parent) = log_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                    use std::io::Write;
                    let _ = writeln!(f, "evt: {evt_line}");
                }
                return 1;
            }
            eprintln!("[agentplug:{plugin} L{level}] {msg}");
            1
        },
    )?;

    linker.func_wrap(
        "env",
        "host_env_get",
        |mut caller: Caller<'_, HostState>, key_ptr: u32, key_len: u32| -> u64 {
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            match std::env::var(&key) {
                Ok(val) => write_guest_bytes(&mut caller, val.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_random_fill",
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> u32 {
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut buf = vec![0u8; len as usize];
            let mut seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15)
                ^ (std::process::id() as u64).wrapping_mul(0xBF58476D1CE4E5B9);
            for byte in buf.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = (seed & 0xff) as u8;
            }
            let memory = guest_memory(&mut caller);
            if memory.write(&mut caller, ptr as usize, &buf).is_err() {
                return 0;
            }
            1
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fetch",
        |mut caller: Caller<'_, HostState>, url_ptr: u32, url_len: u32, opts_ptr: u32, opts_len: u32| -> u64 {
            let url = read_guest_string(&mut caller, url_ptr, url_len);
            let opts_str = read_guest_string(&mut caller, opts_ptr, opts_len);
            let opts: serde_json::Value =
                if opts_str.is_empty() { serde_json::json!({}) } else { serde_json::from_str(&opts_str).unwrap_or(serde_json::json!({})) };
            let method = opts.get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_uppercase();
            let body = opts.get("body").and_then(|v| v.as_str());
            let mut req = fetch_agent().request(&method, &url);
            if let Some(headers) = opts.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in headers {
                    if let Some(vs) = v.as_str() {
                        req = req.set(k, vs);
                    }
                }
            }
            if let Some(timeout_ms) = opts.get("timeoutMs").and_then(|v| v.as_u64()) {
                req = req.timeout(std::time::Duration::from_millis(timeout_ms));
            }
            let resp = match body {
                Some(b) => req.send_string(b),
                None => req.call(),
            };
            let result = match resp {
                Ok(r) => {
                    let status = r.status();
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"ok": true, "status": status, "body": text})
                }
                Err(ureq::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"ok": false, "status": code, "body": text})
                }
                Err(e) => serde_json::json!({"ok": false, "status": 0, "error": e.to_string()}),
            };
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_kv_get",
        |mut caller: Caller<'_, HostState>, ns_ptr: u32, ns_len: u32, key_ptr: u32, key_len: u32| -> u64 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            match fs::read_to_string(&path) {
                Ok(content) => write_guest_bytes(&mut caller, content.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_put",
        |mut caller: Caller<'_, HostState>, ns_ptr: u32, ns_len: u32, key_ptr: u32, key_len: u32, val_ptr: u32, val_len: u32| -> u32 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            let val = read_guest_string(&mut caller, val_ptr, val_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&path, val) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_delete",
        |mut caller: Caller<'_, HostState>, ns_ptr: u32, ns_len: u32, key_ptr: u32, key_len: u32| -> u32 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            match fs::remove_file(&path) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_query",
        |mut caller: Caller<'_, HostState>, ns_ptr: u32, ns_len: u32, q_ptr: u32, q_len: u32| -> u64 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let q = read_guest_string(&mut caller, q_ptr, q_len).to_lowercase();
            if ns.is_empty() {
                return 0;
            }
            let dir = kv_namespace_dir(&caller.data().cwd(), &ns);
            let mut results = Vec::new();
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(content) = fs::read_to_string(&path) {
                        if q.is_empty() || content.to_lowercase().contains(&q) {
                            let key = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
                            results.push(serde_json::json!({"key": key, "value": content}));
                        }
                    }
                }
            }
            write_guest_json(&mut caller, serde_json::json!(results))
        },
    )?;

    linker.func_wrap(
        "env",
        "host_exec_js",
        |mut caller: Caller<'_, HostState>, code_ptr: u32, code_len: u32, opts_ptr: u32, opts_len: u32| -> u64 {
            let code = read_guest_string(&mut caller, code_ptr, code_len);
            let opts_str = read_guest_string(&mut caller, opts_ptr, opts_len);
            let opts: serde_json::Value = if opts_str.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&opts_str).unwrap_or(serde_json::json!({}))
            };
            let cwd = caller.data().cwd();
            let result = crate::exec_js::run(&code, &opts, &cwd);
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_vec_search",
        |mut caller: Caller<'_, HostState>, q_ptr: u32, q_len: u32, k: u32| -> u64 {
            let _ = (q_ptr, q_len, k);
            write_guest_json(&mut caller, serde_json::json!({"ok": false, "error": "host_vec_search_unused_guest_runs_libsql_directly"}))
        },
    )?;
    linker.func_wrap(
        "env",
        "host_task_proc",
        |mut caller: Caller<'_, HostState>, a_ptr: u32, a_len: u32, p_ptr: u32, p_len: u32| -> u64 {
            let action = read_guest_string(&mut caller, a_ptr, a_len);
            let params_str = read_guest_string(&mut caller, p_ptr, p_len);
            let params: serde_json::Value = if params_str.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&params_str).unwrap_or(serde_json::json!({}))
            };
            let cwd = caller.data().cwd();
            let result = crate::task::handle(&action, &params, &cwd);
            write_guest_json(&mut caller, result)
        },
    )?;
    linker.func_wrap(
        "env",
        "host_browser_exec",
        |mut caller: Caller<'_, HostState>, body_ptr: u32, body_len: u32, cwd_ptr: u32, cwd_len: u32, sid_ptr: u32, sid_len: u32, opts_ptr: u32, opts_len: u32| -> u64 {
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd_str = read_guest_string(&mut caller, cwd_ptr, cwd_len);
            let sid = read_guest_string(&mut caller, sid_ptr, sid_len);
            let opts = read_guest_string(&mut caller, opts_ptr, opts_len);
            let cwd = if cwd_str.trim().is_empty() {
                caller.data().cwd()
            } else {
                std::path::PathBuf::from(cwd_str)
            };
            let cwd = canonicalize_path_separators_for_stable_keying(&cwd);
            let result = crate::browser::run(&body, &opts, &cwd, &sid);
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_oxi_exec",
        |mut caller: Caller<'_, HostState>, body_ptr: u32, body_len: u32, cwd_ptr: u32, cwd_len: u32, sid_ptr: u32, sid_len: u32, opts_ptr: u32, opts_len: u32| -> u64 {
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd_str = read_guest_string(&mut caller, cwd_ptr, cwd_len);
            let sid = read_guest_string(&mut caller, sid_ptr, sid_len);
            let opts = read_guest_string(&mut caller, opts_ptr, opts_len);
            let cwd = if cwd_str.trim().is_empty() {
                caller.data().cwd()
            } else {
                std::path::PathBuf::from(cwd_str)
            };
            let cwd = canonicalize_path_separators_for_stable_keying(&cwd);
            let siblings = caller.data().siblings();
            let result = crate::oxibrowser_driver::run(&body, &opts, &cwd, &sid, siblings);
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_plugin_call",
        |mut caller: Caller<'_, HostState>,
         plugin_ptr: u32,
         plugin_len: u32,
         verb_ptr: u32,
         verb_len: u32,
         body_ptr: u32,
         body_len: u32|
         -> u64 {
            let plugin = read_guest_string(&mut caller, plugin_ptr, plugin_len);
            let verb = read_guest_string(&mut caller, verb_ptr, verb_len);
            let body = read_guest_string(&mut caller, body_ptr, body_len);

            let caller_plugin = caller.data().plugin_name.clone();
            if !is_well_formed_plugin_name(&plugin) {
                return write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "invalid_plugin_name", "plugin": plugin}),
                );
            }
            if !is_well_formed_verb(&verb) {
                return write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "invalid_verb", "verb": verb}),
                );
            }
            if let Err(reason) = validate_plugin_call_body(&body) {
                return write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "invalid_body", "reason": reason}),
                );
            }
            if !plugin_call_capability_allowed(&caller.data().cwd(), &caller_plugin, &plugin) {
                return write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "capability_denied", "caller": caller_plugin, "plugin": plugin}),
                );
            }

            let caller_siblings = caller.data().siblings();
            let caller_root = caller.data().cwd();
            let sibling_pool = match crate::registry::ensure_sibling_loaded(&caller_root, &caller_siblings, &plugin) {
                Ok(Some(pool)) => pool,
                Ok(None) => {
                    return write_guest_json(
                        &mut caller,
                        serde_json::json!({"ok": false, "error": "unknown_plugin", "plugin": plugin}),
                    );
                }
                Err(e) => {
                    return write_guest_json(
                        &mut caller,
                        serde_json::json!({"ok": false, "error": format!("plugin_load_failed: {e:#}"), "plugin": plugin}),
                    );
                }
            };

            let acquire_start = std::time::Instant::now();
            let acquire_timeout_ms = crate::registry::SharedPluginPool::ACQUIRE_TIMEOUT_MS;
            let mut guard = sibling_pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies");
            if guard.is_none() {
                drop(guard);
                let remaining_ms = acquire_timeout_ms.saturating_sub(acquire_start.elapsed().as_millis() as u64);
                sibling_pool.any_instantiated_within(remaining_ms);
                let remaining_ms = acquire_timeout_ms.saturating_sub(acquire_start.elapsed().as_millis() as u64);
                guard = sibling_pool.acquire_within(remaining_ms).0;
            }
            let result = match guard.as_mut() {
                None => Err(anyhow::anyhow!("plugin_not_loaded_yet")),
                Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, &verb, &body, &caller_root, caller_siblings.clone()),
            };
            if result.is_ok() {
                crate::registry::settle_slot_after_successful_dispatch(&mut guard, &sibling_pool, &caller_root, &plugin, &verb);
            } else {
                sibling_pool.evict_if_swap_pending(&mut guard);
            }
            drop(guard);

            match result {
                Ok(s) if !s.is_empty() => write_guest_bytes(&mut caller, s.as_bytes()),
                Ok(_) => write_guest_json(&mut caller, serde_json::json!({"ok": true})),
                Err(e) if e.to_string() == "plugin_not_loaded_yet" => write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "plugin_not_loaded_yet", "plugin": plugin}),
                ),
                Err(e) => write_guest_json(&mut caller, serde_json::json!({"ok": false, "error": e.to_string(), "plugin": plugin})),
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_vec_embed",
        |mut caller: Caller<'_, HostState>, text_ptr: u32, text_len: u32, out_ptr: u32, out_len: u32| -> i32 {
            let text = read_guest_string(&mut caller, text_ptr, text_len);
            if text.is_empty() || text.len() > 1024 * 1024 {
                return -1;
            }
            let caller_plugin = caller.data().plugin_name.clone();
            if !plugin_call_capability_allowed(&caller.data().cwd(), &caller_plugin, "bert") {
                eprintln!("[agentplug:host_vec_embed] capability_denied: caller {caller_plugin} not permitted to reach bert");
                return -1;
            }
            let body = serde_json::json!({"text": text}).to_string();

            let caller_siblings = caller.data().siblings();
            let caller_root = caller.data().cwd();
            let sibling_pool = match crate::registry::ensure_sibling_loaded(&caller_root, &caller_siblings, "bert") {
                Ok(Some(pool)) => pool,
                Ok(None) => return -1,
                Err(e) => {
                    eprintln!("[agentplug:host_vec_embed] bert could not be instantiated on first use: {e:#}");
                    return -1;
                }
            };
            const EMBED_RETRY_ATTEMPTS: u32 = 3;
            const EMBED_RETRY_BACKOFF_MS: u64 = 500;
            let mut result: anyhow::Result<Vec<f32>> = Err(anyhow::anyhow!("embed not attempted"));
            for attempt in 0..EMBED_RETRY_ATTEMPTS {
                let mut guard = sibling_pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies");
                result = match guard.as_mut() {
                    None => Err(anyhow::anyhow!("bert not loaded yet")),
                    Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, "embed", &body, &caller_root, caller_siblings.clone()).and_then(|resp| {
                        let v: serde_json::Value = serde_json::from_str(&resp)?;
                        let arr = v.get("embedding").and_then(|e| e.as_array()).ok_or_else(|| anyhow::anyhow!("no embedding field"))?;
                        Ok::<Vec<f32>, anyhow::Error>(arr.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect())
                    }),
                };
                if result.is_err() {
                    *guard = None;
                } else {
                    crate::registry::settle_slot_after_successful_dispatch(&mut guard, &sibling_pool, &caller_root, "bert", "embed");
                }
                drop(guard);
                if result.is_ok() {
                    break;
                }
                if attempt + 1 < EMBED_RETRY_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(EMBED_RETRY_BACKOFF_MS));
                }
            }

            match result {
                Ok(values) => {
                    let dim = values.len().min(out_len as usize);
                    let mut bytes = Vec::with_capacity(dim * 4);
                    for v in &values[..dim] {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    let memory = guest_memory(&mut caller);
                    if memory.write(&mut caller, out_ptr as usize, &bytes).is_err() {
                        return -1;
                    }
                    dim as i32
                }
                Err(e) => {
                    eprintln!("[agentplug:host_vec_embed] failed after {EMBED_RETRY_ATTEMPTS} attempts: {e}");
                    -1
                }
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_git",
        |mut caller: Caller<'_, HostState>, args_ptr: u32, args_len: u32, cwd_ptr: u32, cwd_len: u32| -> u64 {
            let args = read_guest_string(&mut caller, args_ptr, args_len);
            let cwd_arg = read_guest_string(&mut caller, cwd_ptr, cwd_len);
            let trimmed = args.trim();
            let argv: Vec<String> = if trimmed.starts_with('[') {
                serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_else(|_| trimmed.split_whitespace().map(String::from).collect())
            } else {
                trimmed.split_whitespace().map(String::from).collect()
            };
            let cwd = if cwd_arg.is_empty() {
                caller.data().cwd()
            } else {
                let p = PathBuf::from(&cwd_arg);
                if p.is_absolute() { p } else { caller.data().cwd().join(p) }
            };
            if !cwd.is_dir() {
                let v = serde_json::json!({
                    "stdout": "",
                    "stderr": format!(
                        "git cwd does not exist: {} (resolved from cwd arg {:?}) -- pass an absolute path, or a path relative to the dispatch's own working directory, not a bare project/repo name",
                        cwd.display(), cwd_arg
                    ),
                    "exit_code": -1,
                });
                return write_guest_json(&mut caller, v);
            }
            let mut git_cmd = std::process::Command::new("git");
            git_cmd.args(&argv).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                git_cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let v = match git_cmd.spawn() {
                Ok(mut child) => match child.wait_timeout(Duration::from_millis(git_subprocess_timeout_ms())) {
                    Ok(Some(status)) => {
                        let mut stdout = Vec::new();
                        let mut stderr = Vec::new();
                        if let Some(mut o) = child.stdout.take() { let _ = std::io::Read::read_to_end(&mut o, &mut stdout); }
                        if let Some(mut e) = child.stderr.take() { let _ = std::io::Read::read_to_end(&mut e, &mut stderr); }
                        serde_json::json!({
                            "stdout": String::from_utf8_lossy(&stdout),
                            "stderr": String::from_utf8_lossy(&stderr),
                            "exit_code": status.code().unwrap_or(-1),
                        })
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        serde_json::json!({
                            "stdout": "", "stderr": format!("git {argv:?} timed out after {}ms, killed", git_subprocess_timeout_ms()),
                            "exit_code": -1,
                        })
                    }
                    Err(e) => {
                        let _ = child.kill();
                        serde_json::json!({"stdout": "", "stderr": format!("wait_timeout failed: {e}"), "exit_code": -1})
                    }
                },
                Err(e) => serde_json::json!({"stdout": "", "stderr": e.to_string(), "exit_code": 1}),
            };
            write_guest_json(&mut caller, v)
        },
    )?;

    Ok(())
}

fn kv_namespace_dir(cwd: &std::path::Path, ns: &str) -> PathBuf {
    cwd.join(".agentplug-kv").join(safe_name(ns))
}

fn kv_file_path(cwd: &std::path::Path, ns: &str, key: &str) -> PathBuf {
    kv_namespace_dir(cwd, ns).join(format!("{}.json", safe_name(key)))
}

fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' }).collect()
}
