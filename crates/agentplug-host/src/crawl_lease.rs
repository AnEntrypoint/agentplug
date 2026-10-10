use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::crawl::{
    crawl_browser_pid, endpoint_failure, endpoint_ready, find_chrome, free_local_port,
    kill_process_tree, process_exists, reap_stale_crawl_browsers, CDP_POLL_INTERVAL,
    CDP_READY_DEADLINE,
};
use crate::windowless::apply_windowless;

pub const HOUSEKEEPING_TICK: Duration = Duration::from_secs(30);

struct SharedBrowser {
    child: Child,
    pid: u32,
    port: u16,
    profile: PathBuf,
    headful: bool,
    active: HashSet<String>,
}

const CDP_LAUNCH_ATTEMPTS: usize = 2;

static BROWSERS: OnceLock<Mutex<HashMap<PathBuf, SharedBrowser>>> = OnceLock::new();
static HOUSEKEEPING: OnceLock<()> = OnceLock::new();
static AGENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn browsers() -> MutexGuard<'static, HashMap<PathBuf, SharedBrowser>> {
    BROWSERS
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

fn lease_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("browser-leases")
}

fn profiles_root(root: &Path) -> PathBuf {
    root.join(".gm").join("crawl-profiles")
}

fn fresh_profile(root: &Path) -> Result<PathBuf, String> {
    let profile = profiles_root(root).join(format!(
        "cdp-{}-{}",
        std::process::id(),
        AGENT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("could not create Chrome profile {}: {e}", profile.display()))?;
    Ok(profile)
}

fn sweep_old_profiles(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(profiles_root(root)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path != keep {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn lease_path(root: &Path, agent: &str) -> PathBuf {
    lease_dir(root).join(format!("{agent}.json"))
}

pub fn next_agent_id() -> String {
    format!(
        "crawl-{}-{}",
        std::process::id(),
        AGENT_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_lease(root: &Path, agent: &str, port: u16, chrome_pid: u32) -> Result<(), String> {
    let dir = lease_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let body = json!({"agent": agent, "port": port, "chrome_pid": chrome_pid, "pid": std::process::id(), "ts": now_ms()});
    let path = lease_path(root, agent);
    std::fs::write(&path, body.to_string())
        .map_err(|e| format!("could not write lease {}: {e}", path.display()))
}

/// Removes lease files whose agent holds no live lease in this runner. Every
/// crawl runs inside the runner process, so the in-memory active set is the
/// authority; a file without an active owner is left by a dead caller.
fn sweep_stale_leases(root: &Path, active: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(lease_dir(root)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(agent) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if !active.contains(&agent) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn launch(root: &Path, headful: bool) -> Result<SharedBrowser, String> {
    let chrome = find_chrome().ok_or_else(|| {
        "no Chrome found: set GM_BROWSER_CHROME_PATH or CHROME_PATH, or install Google Chrome or Chromium"
            .to_string()
    })?;
    if is_headless_shell(&chrome) {
        return Err(format!(
            "{} is a headless shell, and the cdp engine needs the full Chrome binary: point GM_BROWSER_CHROME_PATH or CHROME_PATH at chrome.exe",
            chrome.display()
        ));
    }
    reap_stale_crawl_browsers(&profiles_root(root));
    let mut last_failure = String::new();
    for attempt in 1..=CDP_LAUNCH_ATTEMPTS {
        let port = free_local_port()?;
        let profile = fresh_profile(root)?;
        sweep_old_profiles(root, &profile);
        let mut cmd = Command::new(&chrome);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()));
        if !headful {
            cmd.arg("--headless=new");
        }
        cmd.args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-default-apps",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        apply_windowless(&mut cmd);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(format!("Chrome launch failed ({}): {e}", chrome.display()));
            }
        };
        let spawn_pid = child.id();
        let mut browser = SharedBrowser {
            child,
            pid: spawn_pid,
            port,
            profile,
            headful,
            active: HashSet::new(),
        };
        if endpoint_ready(port, Instant::now() + CDP_READY_DEADLINE, CDP_POLL_INTERVAL, true) {
            browser.pid = crawl_browser_pid(&browser.profile).unwrap_or(spawn_pid);
            return Ok(browser);
        }
        last_failure = format!(
            "attempt {attempt}/{CDP_LAUNCH_ATTEMPTS}: {} ({}) on port {port} did not answer within {}ms: {}",
            if headful { "headful Chrome" } else { "headless Chrome" },
            chrome.display(),
            CDP_READY_DEADLINE.as_millis(),
            endpoint_failure(port)
        );
        close_browser(&mut browser);
    }
    Err(format!(
        "{last_failure}. Chrome needs a desktop session to open a window: pass `headful` only where one exists, and set GM_BROWSER_CHROME_PATH to a full Chrome otherwise"
    ))
}

fn is_headless_shell(chrome: &Path) -> bool {
    chrome
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("headless"))
}

fn browser_running(browser: &mut SharedBrowser) -> bool {
    if endpoint_ready(browser.port, Instant::now(), CDP_POLL_INTERVAL, true) {
        return true;
    }
    if matches!(browser.child.try_wait(), Ok(None)) {
        return true;
    }
    process_exists(browser.pid)
}

fn close_browser(browser: &mut SharedBrowser) {
    kill_process_tree(browser.pid);
    kill_process_tree(browser.child.id());
    let _ = browser.child.kill();
    let _ = browser.child.wait();
    let _ = std::fs::remove_dir_all(&browser.profile);
}

/// Attaches `agent` to the shared Chrome for `root`, starting it on first use,
/// and returns its DevTools port. Pair every successful call with `release`.
pub fn acquire(root: &Path, agent: &str, headful: bool) -> Result<(u16, u32), String> {
    let mut map = browsers();
    let reusable = match map.get_mut(root) {
        Some(browser) => browser.headful == headful && browser_running(browser),
        None => false,
    };
    if !reusable {
        if let Some(mut stale) = map.remove(root) {
            close_browser(&mut stale);
        }
        let browser = launch(root, headful)?;
        map.insert(root.to_path_buf(), browser);
    }
    let Some(browser) = map.get_mut(root) else {
        return Err("shared Chrome vanished while attaching a lease".to_string());
    };
    sweep_stale_leases(root, &browser.active);
    browser.active.insert(agent.to_string());
    let port = browser.port;
    let chrome_pid = browser.pid;
    if let Err(e) = write_lease(root, agent, port, chrome_pid) {
        let unused = browser.active.remove(agent) && browser.active.is_empty();
        if unused {
            if let Some(mut idle) = map.remove(root) {
                close_browser(&mut idle);
            }
        }
        return Err(e);
    }
    Ok((port, chrome_pid))
}

/// Detaches `agent` and deletes its lease file. When the last lease goes, the
/// shared Chrome closes with it: the run that opened the window closes it.
pub fn release(root: &Path, agent: &str) {
    let mut map = browsers();
    let last_lease_gone = match map.get_mut(root) {
        Some(browser) => {
            browser.active.remove(agent);
            browser.active.is_empty()
        }
        None => false,
    };
    if last_lease_gone {
        if let Some(mut browser) = map.remove(root) {
            close_browser(&mut browser);
        }
    }
    let _ = std::fs::remove_file(lease_path(root, agent));
}

/// Shared-browser state for `root`: liveness, port, pid and held leases.
/// `close_deadline_ms` is always null because the browser closes as soon as
/// its last lease releases.
pub fn status(root: &Path) -> serde_json::Value {
    let mut map = browsers();
    let Some(browser) = map.get_mut(root) else {
        return json!({
            "root": root.display().to_string(),
            "alive": false,
            "lease_count": 0,
            "leases": [],
            "close_deadline_ms": null,
        });
    };
    let alive = browser_running(browser);
    let mut leases: Vec<String> = browser.active.iter().cloned().collect();
    leases.sort();
    json!({
        "root": root.display().to_string(),
        "alive": alive,
        "port": browser.port,
        "chrome_pid": browser.pid,
        "lease_count": leases.len(),
        "leases": leases,
        "close_deadline_ms": null,
    })
}

/// Serves the runner-native verbs `browser_lease_acquire`, `browser_lease_release`
/// and `browser_lease_status`. Body: `{root, agent}`; `root` defaults to the
/// dispatching project root, and `agent` is required by acquire and release.
pub fn lease_reply(verb: &str, dispatch_root: &Path, body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Req {
        root: Option<PathBuf>,
        agent: Option<String>,
    }
    let req: Req = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => {
            return json!({"ok": false, "error": format!("{verb} body must be {{root, agent}}: {e}")})
                .to_string()
        }
    };
    let root = req.root.unwrap_or_else(|| dispatch_root.to_path_buf());
    let reply = match (verb, req.agent.as_deref()) {
        ("browser_lease_acquire", None) | ("browser_lease_release", None) => {
            json!({"ok": false, "error": format!("{verb} needs an agent")})
        }
        ("browser_lease_acquire", Some(agent)) => match acquire(&root, agent, false) {
            Ok((port, chrome_pid)) => json!({
                "ok": true,
                "port": port,
                "chrome_pid": chrome_pid,
                "lease_file": lease_path(&root, agent).display().to_string(),
            }),
            Err(e) => json!({"ok": false, "error": e}),
        },
        ("browser_lease_release", Some(agent)) => {
            release(&root, agent);
            let mut state = status(&root);
            state["released"] = json!(agent);
            state["ok"] = json!(true);
            state
        }
        ("browser_lease_status", _) => {
            let mut state = status(&root);
            state["ok"] = json!(true);
            state
        }
        _ => json!({"ok": false, "error": format!("{verb} is not a browser lease verb")}),
    };
    reply.to_string()
}

/// Closes a shared Chrome that has died, and sweeps leases no live agent owns.
pub fn housekeeping(_now: Instant) {
    let mut map = browsers();
    let mut closing: Vec<PathBuf> = Vec::new();
    for (root, browser) in map.iter_mut() {
        sweep_stale_leases(root, &browser.active);
        if !browser_running(browser) {
            closing.push(root.clone());
        }
    }
    for root in closing {
        if let Some(mut browser) = map.remove(&root) {
            close_browser(&mut browser);
        }
    }
}

/// Starts the housekeeping timer once per process. The runner calls this at
/// startup, next to its other daemon timers.
pub fn spawn_housekeeping_timer() {
    if HOUSEKEEPING.set(()).is_err() {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(HOUSEKEEPING_TICK);
        housekeeping(Instant::now());
    });
}

/// Closes every shared Chrome. Call at runner shutdown.
pub fn shutdown_shared_browsers() {
    for (_, mut browser) in browsers().drain() {
        close_browser(&mut browser);
    }
}
