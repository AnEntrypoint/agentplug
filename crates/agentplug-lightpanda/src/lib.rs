//! Headless crawl engine: one warm `lightpanda serve` per project root.
//!
//! The process is started on the first crawl, kept across dispatches, and
//! reaped after GM_LIGHTPANDA_IDLE_SECONDS (default 300) without a crawl. The
//! CDP target id is remembered so repeat crawls reattach to the same page
//! instead of creating a new one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use agentplug_host::{
    apply_windowless, canonical_project_root, crawl_error_reply, crawl_reply_from_run,
    endpoint_ready, find_on_path, free_local_port, parse_crawl_body, run_helper,
};
use serde_json::{json, Value};

const ENGINE: &str = "lightpanda";
const DEFAULT_IDLE_SECONDS: u64 = 300;
const REAPER_TICK: Duration = Duration::from_secs(5);
const READY_DEADLINE: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(100);
const HELPER_BUDGET: Duration = Duration::from_secs(300);

struct Warm {
    child: Child,
    port: u16,
    target_id: Option<String>,
    last_used: Instant,
    busy: bool,
}

static WARM: OnceLock<Mutex<HashMap<PathBuf, Warm>>> = OnceLock::new();
static REAPER: OnceLock<()> = OnceLock::new();

fn warm_map() -> MutexGuard<'static, HashMap<PathBuf, Warm>> {
    WARM.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn idle_ttl() -> Duration {
    let seconds = std::env::var("GM_LIGHTPANDA_IDLE_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_IDLE_SECONDS);
    Duration::from_secs(seconds)
}

fn resolve_binary() -> Result<PathBuf, String> {
    if let Some(raw) = std::env::var_os("GM_LIGHTPANDA_PATH").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "GM_LIGHTPANDA_PATH names {}, which is not a file",
                path.display()
            ))
        };
    }
    find_on_path("lightpanda").ok_or_else(|| {
        "lightpanda binary not found: set GM_LIGHTPANDA_PATH to the lightpanda binary, or put lightpanda on PATH (lightpanda-io/browser); nothing is downloaded".to_string()
    })
}

fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let last: Vec<&str> = text.lines().rev().take(5).collect();
    let tail = last.into_iter().rev().collect::<Vec<_>>().join(" | ");
    if tail.is_empty() {
        "lightpanda produced no output".to_string()
    } else {
        tail
    }
}

fn start_warm(key: &Path) -> Result<Warm, String> {
    if cfg!(windows) {
        return Err("lightpanda has no native Windows build; run it under WSL2 and point GM_LIGHTPANDA_PATH at a wrapper script".to_string());
    }
    let binary = resolve_binary()?;
    let port = free_local_port()?;
    let dir = key.join(".gm").join("lightpanda");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let log_path = dir.join("serve.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("could not clone the lightpanda log handle: {e}"))?;
    let port_arg = port.to_string();
    let mut cmd = Command::new(&binary);
    cmd.args(["serve", "--host", "127.0.0.1", "--port", port_arg.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    apply_windowless(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("lightpanda launch failed ({}): {e}", binary.display()))?;
    if !endpoint_ready(port, Instant::now() + READY_DEADLINE, READY_POLL) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "lightpanda CDP endpoint on port {port} did not become ready within {}ms (recent output: {})",
            READY_DEADLINE.as_millis(),
            log_tail(&log_path)
        ));
    }
    Ok(Warm {
        child,
        port,
        target_id: None,
        last_used: Instant::now(),
        busy: false,
    })
}

/// Returns the warm port and remembered target for `key`, starting the process
/// when none is alive. Marks the entry busy until `release`.
fn acquire(key: &Path) -> Result<(u16, Option<String>), String> {
    let mut map = warm_map();
    if let Some(warm) = map.get_mut(key) {
        if warm.busy {
            return Err(
                "lightpanda is already running a crawl for this project; retry when it finishes"
                    .to_string(),
            );
        }
        if matches!(warm.child.try_wait(), Ok(None)) {
            warm.busy = true;
            return Ok((warm.port, warm.target_id.clone()));
        }
    }
    if let Some(mut stale) = map.remove(key) {
        let _ = stale.child.kill();
        let _ = stale.child.wait();
    }
    let mut warm = start_warm(key)?;
    warm.busy = true;
    let port = warm.port;
    map.insert(key.to_path_buf(), warm);
    start_reaper();
    Ok((port, None))
}

fn release(key: &Path, target_id: Option<String>) {
    if let Some(warm) = warm_map().get_mut(key) {
        warm.busy = false;
        warm.last_used = Instant::now();
        if target_id.is_some() {
            warm.target_id = target_id;
        }
    }
}

fn start_reaper() {
    if REAPER.set(()).is_err() {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(REAPER_TICK);
        let ttl = idle_ttl();
        let mut map = warm_map();
        let expired: Vec<PathBuf> = map
            .iter_mut()
            .filter(|(_, warm)| !warm.busy)
            .filter_map(|(key, warm)| {
                let idle = warm.last_used.elapsed() >= ttl;
                let exited = !matches!(warm.child.try_wait(), Ok(None));
                (idle || exited).then(|| key.clone())
            })
            .collect();
        for key in expired {
            if let Some(mut warm) = map.remove(&key) {
                let _ = warm.child.kill();
                let _ = warm.child.wait();
            }
        }
    });
}

/// Runs a crawl body on the warm lightpanda for `cwd`'s project root. The
/// body may start with `engine=lightpanda`; `engine=cdp` is refused here.
pub fn crawl(cwd: &Path, body: &str) -> Value {
    let started = Instant::now();
    let parsed = match parse_crawl_body(body) {
        Ok(parsed) => parsed,
        Err(e) => return crawl_error_reply(ENGINE, true, started, e),
    };
    if matches!(parsed.engine.as_deref(), Some(other) if other != ENGINE) {
        return crawl_error_reply(
            ENGINE,
            true,
            started,
            "engine=cdp is served by crawl_cdp in agentplug-host, not the lightpanda plugin"
                .to_string(),
        );
    }
    let key = canonical_project_root(cwd);
    let (port, target_id) = match acquire(&key) {
        Ok(acquired) => acquired,
        Err(e) => return crawl_error_reply(ENGINE, true, started, e),
    };
    let run = run_helper(port, target_id.as_deref(), &parsed.steps, HELPER_BUDGET);
    let remembered = run
        .as_ref()
        .ok()
        .and_then(|r| r.result.as_ref())
        .and_then(|v| v.get("targetId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    release(&key, remembered.or(target_id));
    match run {
        Ok(run) => crawl_reply_from_run(ENGINE, true, started, run),
        Err(e) => crawl_error_reply(ENGINE, true, started, e),
    }
}

/// Plugin verb dispatch. `crawl` takes a crawl body; any other verb answers
/// unknown_verb.
pub fn plugin_call(cwd: &Path, verb: &str, body: &str) -> Value {
    match verb {
        "crawl" => crawl(cwd, body),
        _ => json!({"ok": false, "error": "unknown_verb", "verb": verb}),
    }
}

/// Kills every warm lightpanda process. Call at runner shutdown.
pub fn shutdown() {
    for (_, mut warm) in warm_map().drain() {
        let _ = warm.child.kill();
        let _ = warm.child.wait();
    }
}
