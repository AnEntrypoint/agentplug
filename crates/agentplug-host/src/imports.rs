use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use wait_timeout::ChildExt;
use wasmtime::{AsContextMut, Caller, Linker, Memory};

use crate::host_state::HostState;

// `git log`/`git status`/etc are dispatched synchronously from inside a wasm
// host-import call, itself running while the calling plugin's single pool
// slot (see registry.rs's SharedPluginPool, size 1 for bert/libsql, N for
// gm) is held -- an unbounded git subprocess (pack-refs lock contention, AV
// interference on Windows holding a file handle, a genuinely pathological
// repo) blocks that entire dispatch forever with nothing upstream able to
// notice or recover, wedging every other request for the same project (and,
// for a size-1 shared plugin, every OTHER project) behind it. code_index.rs's
// own INDEX_WALL_BUDGET_MS only checks BETWEEN files in its indexing loop, so
// it never gets a chance to fire while a single host_git call is stuck.
// Bounded the same way browser.rs already bounds its node-helper subprocess
// (wait_timeout + kill-on-expiry) -- git commands here are always
// read-only/local (log/status/diff/rev-parse), so a killed-and-abandoned
// child leaves no repo-state cleanup to do.
const GIT_SUBPROCESS_TIMEOUT_MS: u64 = 15_000;

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

fn write_guest_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
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
    // This runs INSIDE an already-in-flight guest call (e.g. marshaling a
    // slow host_exec_js's result back after the call itself blocked past
    // this Store's epoch deadline) -- the deadline armed at dispatch_on's
    // own entry can legitimately have already elapsed by the time control
    // reaches here, and this alloc call is itself an epoch-checked guest
    // export. A raw .expect() here previously panicked with a wasmtime trap
    // string instead of degrading like every other deadline-exceeded path;
    // `0` is this function's own documented empty/none sentinel (see
    // docs/ABI.md's wire-format section), so a caller already treats it as
    // "no data" rather than crashing on an unexpected return shape. Re-arming
    // here (not just at dispatch_on's own entry) means the NEXT call on this
    // reused Store starts from a fresh budget instead of inheriting this
    // exceeded one.
    match alloc.call(&mut *caller, bytes.len() as u32) {
        Ok(ptr) => {
            let memory = guest_memory(caller);
            if memory.write(&mut *caller, ptr as usize, bytes).is_err() {
                return 0;
            }
            let len = bytes.len() as u64;
            (ptr as u64 & 0xffff_ffff) | (len << 32)
        }
        Err(e) => {
            if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::Interrupt)) {
                caller.as_context_mut().set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(crate::registry::DISPATCH_CALL_DEADLINE_SECS));
            }
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

/// Same `env`-module surface every plugkit-core-derived wasm module already
/// expects (fs/log/env/time/fetch/kv/exec_js/browser/git -- ported verbatim
/// from rs-plugkit's wasm_host.rs), plus `host_plugin_call`: the one new
/// import that makes cross-plugin routing possible. Every OTHER plugin
/// (bert, libsql, tree-sitter) gets this exact same import set too -- a
/// plugin is free to ignore imports it doesn't need, but the host always
/// offers the full surface so any plugin can, in principle, call any other.
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
        "host_fs_read",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let full = caller.data().cwd().join(&path);
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
            let full = caller.data().cwd().join(&path);
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&full, data) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_remove",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let full = caller.data().cwd().join(&path);
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
            let full = caller.data().cwd().join(&path);
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
            let full = caller.data().cwd().join(&path);
            match fs::metadata(&full) {
                Ok(md) => {
                    // mtimeMs lets the guest do a stat-only change check (the
                    // codeinsight digest / per-file skip) without reading and
                    // hashing file content -- the cheap-skip the reference
                    // codebasesearch impl relies on. 0 when the platform can't
                    // report a modified time.
                    let mtime_ms = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    // Both spellings: fs_stat's public shape uses camelCase
                    // (isDirectory/isFile), but the codeinsight guest reads
                    // snake_case `mtime_ms` for its stat-only skip, so emit both
                    // and let each consumer pick the one it expects.
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
            let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
            let req = agent.request(&method, &url);
            let resp = match body {
                Some(b) => req.send_string(b),
                None => req.call(),
            };
            let result = match resp {
                Ok(r) => {
                    let status = r.status();
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"status": status, "body": text})
                }
                Err(ureq::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"status": code, "body": text})
                }
                Err(e) => serde_json::json!({"status": 0, "error": e.to_string()}),
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
                            // Guest-side readers (code_index::load_manifests and
                            // every other fv_query consumer) index each row by
                            // `key`/`value`. Returning the bare content string
                            // made every one of those lookups yield None, so a
                            // fully-populated namespace read back as empty --
                            // 114 valid manifests loaded as 0 chunks, dropping
                            // codesearch to mode:fallback_kv with no hits.
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

    // Three imports genuinely missing from this file's initial port of
    // rs-plugkit's wasm_host.rs -- plugkit.wasm (the "gm" plugin) declares
    // ALL THREE unconditionally at compile time, so a host missing even one
    // fails `WebAssembly.instantiate`/wasmtime's linker.instantiate with a
    // hard "unknown import" error, not a graceful per-call fallback.
    // host_vec_search must stay DECLARED because gm.wasm imports it
    // unconditionally at compile time -- a missing import breaks every dispatch,
    // not just vector calls. But it is no longer CALLED by any guest code path:
    // the guest now runs vector search in-process against libsql (vec_search_local
    // in rs-plugkit verbs.rs), so this import is dead. It returns a typed error
    // only to satisfy the ABI for a call that can never arrive; there is no stub
    // behind a live subsystem here.
    linker.func_wrap(
        "env",
        "host_vec_search",
        |mut caller: Caller<'_, HostState>, q_ptr: u32, q_len: u32, k: u32| -> u64 {
            let _ = (q_ptr, q_len, k);
            write_guest_json(&mut caller, serde_json::json!({"ok": false, "error": "host_vec_search_unused_guest_runs_libsql_directly"}))
        },
    )?;
    // host_task_proc was a not_implemented stub, so task-spawn/task-stop were
    // non-functional under the native runtime -- the whole background-process
    // subsystem was unreachable (task-list worked only because it reads an
    // empty registry). Wired to a real native process registry (crate::task)
    // that spawns detached children, drains their output opportunistically on
    // each list/output/stop, and reaps on exit or timeout. Reuses exec_js's
    // build_command for the lang->command mapping so task and exec_js resolve
    // languages identically.
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
    // host_browser_exec was a documented capability gap -- agentplug-host had
    // no browser module, so every browser-verb dispatch returned a typed
    // not_implemented failure and the whole browser subsystem was advertised in
    // `health` yet non-functional under the native runtime. Ported the same
    // self-contained module gm-runner uses (crate::browser::run, which shells
    // out to the playwriter CLI rather than embedding chromium, so it needs no
    // new deps -- serde_json/directories/wait-timeout were already present).
    // The guest passes body / cwd / session_id; prefer the passed cwd, falling
    // back to the host's own cwd when the guest sends an empty string.
    linker.func_wrap(
        "env",
        "host_browser_exec",
        |mut caller: Caller<'_, HostState>, body_ptr: u32, body_len: u32, cwd_ptr: u32, cwd_len: u32, sid_ptr: u32, sid_len: u32| -> u64 {
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd_str = read_guest_string(&mut caller, cwd_ptr, cwd_len);
            let sid = read_guest_string(&mut caller, sid_ptr, sid_len);
            let cwd = if cwd_str.trim().is_empty() {
                caller.data().cwd()
            } else {
                std::path::PathBuf::from(cwd_str)
            };
            let result = crate::browser::run(&body, &cwd, &sid);
            write_guest_json(&mut caller, result)
        },
    )?;

    // The single new import over the existing gm-runner wasm_host.rs surface:
    // routes to another loaded plugin for the SAME project. Looks up the
    // sibling by name in the shared registry, calls its `plugin_call` export,
    // marshals args in and the result back through the CALLING plugin's
    // memory (never the callee's -- the caller is the one that can read the
    // response afterward).
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

            let caller_siblings = caller.data().siblings();
            let sibling_pool = { caller_siblings.lock().unwrap().get(&plugin).cloned() };
            let Some(sibling_pool) = sibling_pool else {
                return write_guest_json(
                    &mut caller,
                    serde_json::json!({"ok": false, "error": "unknown_plugin", "plugin": plugin}),
                );
            };

            // Drive the call through the SIBLING's own Store, never `caller`
            // (the CALLING plugin's Store) -- wasmtime::Instance methods
            // require a StoreContextMut matching the store the Instance was
            // instantiated in. Reusing `caller` here previously panicked
            // ("object used with the wrong store", wasmtime-46
            // store/data.rs:213) the first time gm.wasm's recall path called
            // into bert via host_plugin_call.
            let caller_root = caller.data().cwd();
            let mut guard = match sibling_pool.acquire() {
                Some(g) => g,
                None => {
                    return write_guest_json(
                        &mut caller,
                        serde_json::json!({"ok": false, "error": "plugin_pool_busy_timeout", "plugin": plugin}),
                    );
                }
            };
            let result = match guard.as_mut() {
                None => Err(anyhow::anyhow!("plugin_not_loaded_yet")),
                Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, &verb, &body, &caller_root, caller_siblings.clone()),
            };
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
            let body = serde_json::json!({"text": text}).to_string();

            // Same fix as host_plugin_call: drive bert's own Store, never
            // `caller` (this plugin's Store) -- see SiblingHandle's doc
            // comment for the wasmtime cross-store panic this replaced.
            let caller_siblings = caller.data().siblings();
            let sibling_pool = { caller_siblings.lock().unwrap().get("bert").cloned() };
            let Some(sibling_pool) = sibling_pool else {
                return -1;
            };
            let caller_root = caller.data().cwd();
            let mut guard = match sibling_pool.acquire() {
                Some(g) => g,
                None => return -1,
            };
            let result = match guard.as_mut() {
                None => Err(anyhow::anyhow!("bert not loaded yet")),
                Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, "embed", &body, &caller_root, caller_siblings.clone()).and_then(|resp| {
                    let v: serde_json::Value = serde_json::from_str(&resp)?;
                    let arr = v.get("embedding").and_then(|e| e.as_array()).ok_or_else(|| anyhow::anyhow!("no embedding field"))?;
                    Ok::<Vec<f32>, anyhow::Error>(arr.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect())
                }),
            };
            drop(guard);

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
                Err(_) => -1,
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
            let cwd = if cwd_arg.is_empty() { caller.data().cwd() } else { PathBuf::from(&cwd_arg) };
            let mut git_cmd = std::process::Command::new("git");
            git_cmd.args(&argv).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                git_cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let v = match git_cmd.spawn() {
                Ok(mut child) => match child.wait_timeout(Duration::from_millis(GIT_SUBPROCESS_TIMEOUT_MS)) {
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
                            "stdout": "", "stderr": format!("git {argv:?} timed out after {GIT_SUBPROCESS_TIMEOUT_MS}ms, killed"),
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
