use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, StoreContextMut};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::imports::{
    atomic_cas_write_locked, atomic_write_locked, base64_encode, configure_github_git_credentials,
    create_private_dir_all, fetch_agent, git_subprocess_timeout_ms_for, has_project_marker, kv_file_path, normalize_lexically,
    plugin_call_capability_allowed, sandboxed_guest_path_with_extra_roots,
    validate_plugin_call_body,
};

pub const GM_ENV_INTERFACE: &str = "gm:plugkit/env@0.1.0";
pub const GM_GUEST_INTERFACE: &str = "gm:plugkit/guest@0.1.0";
static COMPONENT_ENGINE: OnceLock<Engine> = OnceLock::new();

const BINARY_READ_MAX_BYTES: u64 = 20 * 1024 * 1024;
const GIT_STDOUT_CAP_BYTES: usize = 1_048_576;
const GIT_STDERR_CAP_BYTES: usize = 262_144;

pub struct ComponentState {
    ctx: WasiCtx,
    table: ResourceTable,
    cwd: PathBuf,
    extra_roots: Vec<PathBuf>,
}

impl ComponentState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            cwd,
            extra_roots: Vec::new(),
        }
    }

    fn resolve(&self, path: &str) -> Option<PathBuf> {
        sandboxed_guest_path_with_extra_roots(&self.cwd, path, &self.extra_roots)
    }
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

pub fn build_component_engine() -> anyhow::Result<Engine> {
    if let Some(engine) = COMPONENT_ENGINE.get() {
        return Ok(engine.clone());
    }
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|e| anyhow::anyhow!(e))?;
    let ticker = engine.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(crate::EPOCH_TICK_INTERVAL_MS));
        ticker.increment_epoch();
    });
    Ok(COMPONENT_ENGINE.get_or_init(|| engine).clone())
}

fn parse_json_object(text: &str) -> serde_json::Value {
    if text.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({}))
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        ^ (std::process::id() as u64).wrapping_mul(0xBF58476D1CE4E5B9);
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed & 0xff) as u8
        })
        .collect()
}

fn fetch(url: &str, opts_text: &str) -> serde_json::Value {
    let opts = parse_json_object(opts_text);
    let method = opts
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_uppercase();
    let body = opts.get("body").and_then(|v| v.as_str());
    let timeout = opts
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10));
    let binary_response = opts.get("responseEncoding").and_then(|v| v.as_str()) == Some("base64");
    let agent = if binary_response {
        crate::http_agent::build_agent_without_redirects(timeout)
    } else {
        fetch_agent(timeout)
    };
    let mut req = agent.request(&method, url);
    if let Some(headers) = opts.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in headers {
            if let Some(vs) = v.as_str() {
                req = req.set(k, vs);
            }
        }
    }
    req = req.timeout(timeout);
    let resp = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    match resp {
        Ok(r) if binary_response => {
            let status = r.status();
            let content_type = r.header("content-type").unwrap_or("").to_string();
            let location = r.header("location").map(String::from);
            let mut bytes = Vec::new();
            match r
                .into_reader()
                .take(BINARY_READ_MAX_BYTES + 1)
                .read_to_end(&mut bytes)
            {
                Ok(_) if bytes.len() as u64 > BINARY_READ_MAX_BYTES => serde_json::json!({"ok": false, "status": status, "error": "response_exceeds_binary_limit", "limit_bytes": BINARY_READ_MAX_BYTES}),
                Ok(_) => serde_json::json!({"ok": true, "status": status, "content_type": content_type, "location": location, "body": base64_encode(&bytes), "encoding": "base64"}),
                Err(e) => serde_json::json!({"ok": false, "status": status, "error": e.to_string()}),
            }
        }
        Ok(r) => {
            let status = r.status();
            let text = r.into_string().unwrap_or_default();
            serde_json::json!({"ok": true, "status": status, "body": text})
        }
        Err(ureq::Error::Status(code, r)) => {
            let location = r.header("location").map(String::from);
            let text = r.into_string().unwrap_or_default();
            serde_json::json!({"ok": false, "status": code, "body": text, "location": location})
        }
        Err(e) => serde_json::json!({"ok": false, "status": 0, "error": e.to_string()}),
    }
}

fn run_git(root: &Path, args: &str, cwd_arg: &str) -> serde_json::Value {
    let trimmed = args.trim();
    let argv: Vec<String> = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<String>>(trimmed)
            .unwrap_or_else(|_| trimmed.split_whitespace().map(String::from).collect())
    } else {
        trimmed.split_whitespace().map(String::from).collect()
    };
    let cwd = if cwd_arg.is_empty() {
        root.to_path_buf()
    } else {
        let requested = PathBuf::from(cwd_arg);
        if requested.is_absolute() { requested } else { root.join(requested) }
    };
    if !cwd.is_dir() {
        return serde_json::json!({
            "stdout": "",
            "stderr": format!(
                "git cwd does not exist: {} (resolved from cwd arg {:?}) -- pass an absolute path, or a path relative to the dispatch's own working directory, not a bare project/repo name",
                cwd.display(), cwd_arg
            ),
            "exit_code": -1,
        });
    }
    let mut command = Command::new("git");
    command
        .args(&argv)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_github_git_credentials(&mut command, &argv);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        crate::windowless::apply_windowless(&mut command);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return serde_json::json!({"stdout": "", "stderr": e.to_string(), "exit_code": 1}),
    };
    let containment = match crate::process_tree::establish_containment(&child) {
        Ok(containment) => containment,
        Err(error) => {
            crate::process_tree::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return serde_json::json!({
                "stdout": "",
                "stderr": format!("git process containment failed: {error}"),
                "exit_code": -1,
            });
        }
    };
    let cap_hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out_handle = child.stdout.take().map(|o| crate::imports::drain_reader_capped(o, GIT_STDOUT_CAP_BYTES, cap_hit.clone()));
    let err_handle = child.stderr.take().map(|e| crate::imports::drain_reader_capped(e, GIT_STDERR_CAP_BYTES, cap_hit.clone()));
    let timeout_ms = git_subprocess_timeout_ms_for(&argv);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut exit_code: i32 = -1;
    let mut timed_out = false;
    let mut cap_stopped = false;
    loop {
        if cap_hit.load(std::sync::atomic::Ordering::SeqCst) {
            crate::process_tree::terminate_containment(&containment, child.id());
            let _ = child.wait();
            cap_stopped = true;
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            crate::process_tree::terminate_containment(&containment, child.id());
            let _ = child.wait();
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (stdout, out_truncated) = out_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    let (stderr, err_truncated) = err_handle.and_then(|h| h.join().ok()).unwrap_or_default();
    let output_truncated = out_truncated || err_truncated || cap_stopped;
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    if timed_out {
        return serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": format!("git {argv:?} timed out after {timeout_ms}ms, killed"),
            "exit_code": -1,
        });
    }
    let limit_error = format!("git {argv:?} output exceeded the host capture limit");
    serde_json::json!({
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": if output_truncated {
            if stderr.is_empty() { limit_error } else { format!("{stderr}\n{limit_error}") }
        } else {
            stderr
        },
        "exit_code": if output_truncated { -1 } else { exit_code },
        "stdout_truncated": out_truncated,
        "stderr_truncated": err_truncated,
    })
}

fn is_well_formed_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_well_formed_verb(verb: &str) -> bool {
    !verb.is_empty()
        && verb.len() <= 128
        && verb.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn plugin_call_reply(store_cwd: &Path, plugin: &str, verb: &str, body: &str) -> serde_json::Value {
    if !is_well_formed_plugin_name(plugin) {
        return serde_json::json!({"ok": false, "error": "invalid_plugin_name", "plugin": plugin});
    }
    if !is_well_formed_verb(verb) {
        return serde_json::json!({"ok": false, "error": "invalid_verb", "verb": verb});
    }
    if let Err(reason) = validate_plugin_call_body(body) {
        return serde_json::json!({"ok": false, "error": "invalid_body", "reason": reason});
    }
    if !plugin_call_capability_allowed(store_cwd, "", plugin) {
        return serde_json::json!({"ok": false, "error": "capability_denied", "caller": "", "plugin": plugin});
    }
    serde_json::json!({"ok": false, "error": "unknown_plugin", "plugin": plugin})
}

pub fn component_linker(engine: &Engine) -> anyhow::Result<Linker<ComponentState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut env = linker.instance(GM_ENV_INTERFACE)?;
    env.func_wrap(
        "host-cwd",
        |store: StoreContextMut<'_, ComponentState>, (): ()| {
            Ok((store.data().cwd.to_str().map(str::to_owned),))
        },
    )?;
    env.func_wrap(
        "host-fs-allow-root",
        |mut store: StoreContextMut<'_, ComponentState>, (path,): (String,)| {
            let allowed = match normalize_lexically(Path::new(&path)) {
                Some(root) => match std::fs::metadata(&root) {
                    Ok(md) if md.is_dir() && has_project_marker(&root) => {
                        let state = store.data_mut();
                        if !state.extra_roots.contains(&root) {
                            state.extra_roots.push(root);
                        }
                        true
                    }
                    _ => false,
                },
                None => false,
            };
            Ok((allowed,))
        },
    )?;
    env.func_wrap(
        "host-fs-read",
        |store: StoreContextMut<'_, ComponentState>, (path,): (String,)| {
            let content = store
                .data()
                .resolve(&path)
                .and_then(|full| std::fs::read_to_string(full).ok());
            Ok((content,))
        },
    )?;
    env.func_wrap(
        "host-fs-write",
        |store: StoreContextMut<'_, ComponentState>, (path, data): (String, String)| {
            let written = store.data().resolve(&path).is_some_and(|full| {
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                atomic_write_locked(&full, &data).is_ok()
            });
            Ok((written,))
        },
    )?;
    env.func_wrap(
        "host-fs-cas-write",
        |store: StoreContextMut<'_, ComponentState>,
         (path, expected, data): (String, String, String)| {
            let code = match store.data().resolve(&path) {
                None => 0u32,
                Some(full) => {
                    if let Some(parent) = full.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match atomic_cas_write_locked(&full, &expected, &data) {
                        Ok(true) => 1,
                        Ok(false) => 2,
                        Err(_) => 0,
                    }
                }
            };
            Ok((code,))
        },
    )?;
    env.func_wrap(
        "host-fs-remove",
        |store: StoreContextMut<'_, ComponentState>, (path,): (String,)| {
            let removed = store.data().resolve(&path).is_some_and(|full| match std::fs::metadata(&full) {
                Ok(md) if md.is_dir() => false,
                Ok(_) => std::fs::remove_file(&full).is_ok(),
                Err(_) => false,
            });
            Ok((removed,))
        },
    )?;
    env.func_wrap(
        "host-fs-readdir",
        |store: StoreContextMut<'_, ComponentState>, (path,): (String,)| {
            let listing = store
                .data()
                .resolve(&path)
                .and_then(|full| {
                    std::fs::read_dir(&full)
                        .and_then(|rd| {
                            rd.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                                .collect::<std::io::Result<Vec<String>>>()
                        })
                        .ok()
                })
                .map(|entries| serde_json::json!(entries).to_string());
            Ok((listing,))
        },
    )?;
    env.func_wrap(
        "host-fs-stat",
        |store: StoreContextMut<'_, ComponentState>, (path,): (String,)| {
            let stat = store.data().resolve(&path).and_then(|full| {
                let md = std::fs::metadata(&full).ok()?;
                let mtime_ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let canonical_path = std::fs::canonicalize(&full)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                Some(serde_json::json!({"isDirectory": md.is_dir(), "isFile": md.is_file(), "size": md.len(), "mtimeMs": mtime_ms, "mtime_ms": mtime_ms, "canonicalPath": canonical_path}).to_string())
            });
            Ok((stat,))
        },
    )?;
    env.func_wrap(
        "host-fetch",
        |_: StoreContextMut<'_, ComponentState>, (url, opts): (String, String)| {
            Ok((Some(fetch(&url, &opts).to_string()),))
        },
    )?;
    env.func_wrap(
        "host-kv-get",
        |store: StoreContextMut<'_, ComponentState>, (ns, key): (String, String)| {
            let value = if ns.is_empty() || key.is_empty() {
                None
            } else {
                std::fs::read_to_string(kv_file_path(&store.data().cwd, &ns, &key)).ok()
            };
            Ok((value,))
        },
    )?;
    env.func_wrap(
        "host-kv-put",
        |store: StoreContextMut<'_, ComponentState>, (ns, key, val): (String, String, String)| {
            let code = if ns.is_empty() || key.is_empty() {
                0u32
            } else {
                let path = kv_file_path(&store.data().cwd, &ns, &key);
                if let Some(parent) = path.parent() {
                    let _ = create_private_dir_all(parent);
                }
                u32::from(crate::imports::create_private_file(&path, &val).is_ok())
            };
            Ok((code,))
        },
    )?;
    env.func_wrap(
        "host-kv-delete",
        |store: StoreContextMut<'_, ComponentState>, (ns, key): (String, String)| {
            let code = if ns.is_empty() || key.is_empty() {
                0u32
            } else {
                u32::from(std::fs::remove_file(kv_file_path(&store.data().cwd, &ns, &key)).is_ok())
            };
            Ok((code,))
        },
    )?;
    env.func_wrap(
        "host-kv-query",
        |store: StoreContextMut<'_, ComponentState>, (ns, query): (String, String)| {
            if ns.is_empty() {
                return Ok((None::<String>,));
            }
            let query = query.to_lowercase();
            let dir = kv_file_path(&store.data().cwd, &ns, "").parent().map(Path::to_path_buf);
            let mut results = Vec::new();
            if let Some(entries) = dir.and_then(|dir| std::fs::read_dir(dir).ok()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if query.is_empty() || content.to_lowercase().contains(&query) {
                            let key = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or_default()
                                .to_string();
                            results.push(serde_json::json!({"key": key, "value": content}));
                        }
                    }
                }
            }
            Ok((Some(serde_json::json!(results).to_string()),))
        },
    )?;
    env.func_wrap(
        "host-vec-embed",
        |_: StoreContextMut<'_, ComponentState>, (_text, _out_len): (String, u32)| {
            Ok(((-1i32, Vec::<f32>::new()),))
        },
    )?;
    env.func_wrap(
        "host-exec-js",
        |store: StoreContextMut<'_, ComponentState>, (code, opts): (String, String)| {
            let result = crate::exec_js::run(&code, &parse_json_object(&opts), &store.data().cwd);
            Ok((Some(result.to_string()),))
        },
    )?;
    env.func_wrap(
        "host-log",
        |store: StoreContextMut<'_, ComponentState>, (level, msg): (u32, String)| {
            if let Some(evt_line) = msg.strip_prefix("evt: ") {
                crate::watcher_log::append_watcher_line(&store.data().cwd, &format!("evt: {evt_line}"));
            } else {
                eprintln!("[agentplug component] level={level} {msg}");
            }
            Ok((1u32,))
        },
    )?;
    env.func_wrap(
        "host-now-ms",
        |_: StoreContextMut<'_, ComponentState>, (): ()| Ok((crate::now_ms(),)),
    )?;
    env.func_wrap(
        "host-env-get",
        |_: StoreContextMut<'_, ComponentState>, (key,): (String,)| {
            let value = match crate::dispatch_origin::dispatch_env_value(&key) {
                Some(value) => value,
                None => std::env::var(&key).ok(),
            };
            Ok((value,))
        },
    )?;
    env.func_wrap(
        "host-random-fill",
        |_: StoreContextMut<'_, ComponentState>, (len,): (u32,)| {
            Ok(((1u32, random_bytes(len as usize)),))
        },
    )?;
    env.func_wrap(
        "host-task-proc",
        |store: StoreContextMut<'_, ComponentState>, (action, params): (String, String)| {
            let params = parse_json_object(&params);
            let cwd = store.data().cwd.clone();
            let result = if action == crate::fs_prewarm::ACTION {
                let roots = store.data().extra_roots.clone();
                crate::fs_prewarm::run(&params, |p| sandboxed_guest_path_with_extra_roots(&cwd, p, &roots))
            } else {
                crate::task::handle(&action, &params, &cwd)
            };
            Ok((Some(result.to_string()),))
        },
    )?;
    env.func_wrap(
        "host-git",
        |store: StoreContextMut<'_, ComponentState>, (args, cwd_arg): (String, String)| {
            let reply = run_git(&store.data().cwd, &args, &cwd_arg);
            Ok((Some(reply.to_string()),))
        },
    )?;
    env.func_wrap(
        "host-plugin-call",
        |store: StoreContextMut<'_, ComponentState>, (plugin, verb, body): (String, String, String)| {
            let reply = plugin_call_reply(&store.data().cwd, &plugin, &verb, &body);
            Ok((Some(reply.to_string()),))
        },
    )?;
    env.func_wrap(
        "browser-cdp",
        |store: StoreContextMut<'_, ComponentState>, (body,): (String,)| {
            let reply = crate::browser::browser_cdp(&store.data().cwd, &body);
            Ok((Some(reply.to_string()),))
        },
    )?;
    env.func_wrap(
        "browser-events",
        |store: StoreContextMut<'_, ComponentState>, (body,): (String,)| {
            let reply = browser_events_host::reply(&store.data().cwd, &body);
            Ok((Some(reply.to_string()),))
        },
    )?;
    env.func_wrap(
        "browser-ops",
        |store: StoreContextMut<'_, ComponentState>, (op, body): (String, String)| {
            let reply = crate::browser_ops::reply(&store.data().cwd, &op, &body);
            Ok((Some(reply.to_string()),))
        },
    )?;
    Ok(linker)
}

pub async fn dispatch_component_verb(
    engine: &Engine,
    component: &wasmtime::component::Component,
    linker: &Linker<ComponentState>,
    plugin_name: &str,
    cwd: &Path,
    verb: &str,
    body: &str,
) -> anyhow::Result<String> {
    let deadline_secs = crate::registry::deadline_secs_for_call(plugin_name, body);
    let mut store = wasmtime::Store::new(engine, ComponentState::new(cwd.to_path_buf()));
    store.set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(deadline_secs));
    let instance = linker.instantiate_async(&mut store, component).await?;
    let guest = instance
        .get_export_index(&mut store, None, GM_GUEST_INTERFACE)
        .ok_or_else(|| anyhow::anyhow!("component exports no {GM_GUEST_INTERFACE} interface"))?;
    let dispatch = instance
        .get_export_index(&mut store, Some(&guest), "dispatch-verb")
        .ok_or_else(|| anyhow::anyhow!("{GM_GUEST_INTERFACE} exports no dispatch-verb function"))?;
    let dispatch_fn = instance.get_typed_func::<(&str, &str), (String,)>(&mut store, &dispatch)?;
    let (reply,) = match dispatch_fn.call_async(&mut store, (verb, body)).await {
        Ok(result) => result,
        Err(e) => {
            if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::Interrupt)) {
                return Err(anyhow::anyhow!("plugin_call_deadline_exceeded: {plugin_name} exceeded {deadline_secs}s executing verb {verb}"));
            }
            return Err(e.into());
        }
    };
    if reply.is_empty() {
        return Err(anyhow::anyhow!("plugin_response_missing: {plugin_name} verb {verb} returned an empty string from the component dispatch-verb export"));
    }
    Ok(reply)
}

// Live feed host side: the body is JSON {"action":"start"|"status"|"stop","session":"<id>","url"?,"maxMs"?}.
// Start takes the project's browser lease and keeps it renewed with attend_while_running for as long as the
// node helper runs (refresh_heartbeat accepts only this process as holder, so the heartbeat lives here).
// Stop writes the stop marker by temp name and rename; the helper exits and the lease is released.
pub(crate) mod browser_events_host {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};

    use crate::browser_lease;

    const SCRIPT: &str = include_str!("browser_events.mjs");
    const DEFAULT_MAX_MS: u64 = 900_000;
    const MAX_MAX_MS: u64 = 3_600_000;
    const LIVE_HEARTBEAT_MS: u64 = 10_000;

    struct Feed {
        done: Arc<AtomicBool>,
        stop: PathBuf,
    }

    fn feeds() -> MutexGuard<'static, HashMap<String, Feed>> {
        static FEEDS: OnceLock<Mutex<HashMap<String, Feed>>> = OnceLock::new();
        FEEDS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn fail(message: &str) -> Value {
        json!({ "ok": false, "error": message })
    }

    fn valid_session(session: &str) -> bool {
        !session.is_empty()
            && session.len() <= 64
            && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    fn spool_dir(root: &Path) -> PathBuf {
        root.join(".gm").join("browser-events")
    }

    fn is_running(session: &str) -> bool {
        feeds()
            .get(session)
            .is_some_and(|f| !f.done.load(Ordering::Relaxed))
    }

    fn start(root: &Path, req: &Value, session: &str) -> Value {
        if is_running(session) {
            return fail("a live feed already runs for this session");
        }
        let Some(node) = crate::browser::find_on_path("node") else {
            return fail("node is required on PATH to run the browser events feed");
        };
        let dir = spool_dir(root);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return fail(&format!("could not create {}: {e}", dir.display()));
        }
        let spool = dir.join(format!("{session}.jsonl"));
        let stop = dir.join(format!("{session}.jsonl.stop"));
        let lock = dir.join(format!("{session}.jsonl.lock"));
        let status_path = dir.join(format!("{session}.jsonl.status.json"));
        // A lock whose helper heartbeat is recent belongs to a live feed in another host process; only a
        // stale lock (a crashed helper) is cleared.
        let beat = std::fs::read_to_string(&status_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.get("heartbeatAt").and_then(Value::as_u64))
            .unwrap_or(0);
        if now_ms().saturating_sub(beat) < LIVE_HEARTBEAT_MS {
            return fail("a live feed in another process holds this spool");
        }
        let _ = std::fs::remove_file(&lock);
        let _ = std::fs::remove_file(&stop);

        let agent = browser_lease::next_agent_id();
        let (port, _) = match browser_lease::acquire(root, &agent, true) {
            Ok(attached) => attached,
            Err(e) => return fail(&e),
        };
        let attendance = browser_lease::attend_while_running(root);
        let max_ms = req
            .get("maxMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_MS)
            .clamp(1, MAX_MAX_MS);
        let config = json!({
            "port": port,
            "session": session,
            "spool": spool.to_string_lossy(),
            "url": req.get("url").and_then(Value::as_str),
            "maxMs": max_ms,
        });
        let work_dir = if root.is_dir() {
            root.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let mut cmd = Command::new(node);
        cmd.arg("--input-type=module")
            .current_dir(&work_dir)
            .env("GM_BROWSER_EVENTS_CONFIG", config.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::windowless::apply_windowless(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                browser_lease::release(root, &agent);
                return fail(&format!("could not start node: {e}"));
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(SCRIPT.as_bytes());
        }

        let done = Arc::new(AtomicBool::new(false));
        let thread_done = Arc::clone(&done);
        let root_owned = root.to_path_buf();
        let agent_owned = agent.clone();
        let lock_owned = lock.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            drop(attendance);
            browser_lease::release(&root_owned, &agent_owned);
            let _ = std::fs::remove_file(&lock_owned);
            thread_done.store(true, Ordering::Relaxed);
        });
        {
            let mut map = feeds();
            map.retain(|_, f| !f.done.load(Ordering::Relaxed));
            map.insert(session.to_string(), Feed { done, stop });
        }
        json!({
            "ok": true,
            "action": "start",
            "session": session,
            "spool": spool.to_string_lossy(),
            "port": port,
            "maxMs": max_ms,
        })
    }

    fn stop(session: &str) -> Value {
        let map = feeds();
        let Some(feed) = map.get(session) else {
            return fail("no feed runs for this session");
        };
        if feed.done.load(Ordering::Relaxed) {
            return json!({ "ok": true, "action": "stop", "session": session, "stopping": false });
        }
        let tmp = PathBuf::from(format!("{}.tmp", feed.stop.display()));
        if let Err(e) = std::fs::write(&tmp, b"stop\n").and_then(|_| std::fs::rename(&tmp, &feed.stop)) {
            return fail(&format!("could not write the stop marker: {e}"));
        }
        json!({ "ok": true, "action": "stop", "session": session, "stopping": true })
    }

    fn status(root: &Path, session: &str) -> Value {
        let status_path = spool_dir(root).join(format!("{session}.jsonl.status.json"));
        let helper = std::fs::read_to_string(&status_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        json!({
            "ok": true,
            "action": "status",
            "session": session,
            "running": is_running(session),
            "status": helper,
        })
    }

    pub fn reply(root: &Path, body: &str) -> Value {
        let req: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => return fail(&format!("body must be JSON: {e}")),
        };
        let session = req.get("session").and_then(Value::as_str).unwrap_or("");
        if !valid_session(session) {
            return fail("session must be 1 to 64 characters of letters, digits, _ or -");
        }
        match req.get("action").and_then(Value::as_str).unwrap_or("start") {
            "start" => start(root, &req, session),
            "stop" => stop(session),
            "status" => status(root, session),
            other => fail(&format!("unknown action {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::Component;
    use wasmtime::Store;

    #[test]
    #[ignore = "needs the wasip3 component artifact built from rs-plugkit"]
    fn component_links_wasi_p3_and_every_env_import() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let path = std::env::var("GM_WASIP3_COMPONENT").map(PathBuf::from).unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../rs-plugkit/target/wasm32-wasip3/release/rs_plugkit.wasm")
            });
            let engine = build_component_engine().unwrap();
            let component = Component::from_file(&engine, &path).unwrap();
            let linker = component_linker(&engine).unwrap();
            let mut store = Store::new(&engine, ComponentState::new(std::env::temp_dir()));
            store.set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(crate::registry::DISPATCH_CALL_DEADLINE_SECS));
            linker
                .instantiate_async(&mut store, &component)
                .await
                .expect("every gm:plugkit/env import must be provided by component_linker");
        });
    }

    #[test]
    #[ignore = "needs the wasip3 component artifact built from rs-plugkit"]
    fn component_dispatches_health_through_registry() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let path = std::env::var("GM_WASIP3_COMPONENT").map(PathBuf::from).unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../rs-plugkit/target/wasm32-wasip3/release/rs_plugkit.wasm")
            });
            let core_engine = crate::build_engine().unwrap();
            let component_engine = build_component_engine().unwrap();
            let artifact = crate::loader::load_selected_artifact(&core_engine, &component_engine, &path, "rs-plugkit", "").unwrap();
            let reply = artifact
                .dispatch_verb(&component_engine, "rs-plugkit", &std::env::temp_dir(), "health", "{}")
                .await
                .unwrap();
            assert!(!reply.is_empty(), "health reply is empty");
            assert!(reply.starts_with('{'), "health reply is not a JSON object: {reply}");
            let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(value["verb"], "health");
            assert!(value["data"]["crate_version"].is_string());
        });
    }
}
