use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::crawl::{
    crawl_browser_pid, endpoint_failure, endpoint_ready, find_chrome, free_local_port,
    kill_process_tree, process_exists, reap_stale_crawl_browsers, CDP_POLL_INTERVAL,
    CDP_READY_DEADLINE,
};
use crate::windowless::apply_windowless;

pub const HOUSEKEEPING_TICK: Duration = Duration::from_secs(30);

pub const HEARTBEAT_MS: u64 = 2_000;
pub const TTL_MS: u64 = 120_000;
pub const WAIT_MS: u64 = 600_000;
const POLL_MS: u64 = 500;
const CLAIM_CREATE_GRACE_MS: u64 = 5_000;

const SESSION: &str = "session";
const PROFILE: &str = "profile";
const OWNER_FILE: &str = "owner.json";

struct SharedBrowser {
    child: Child,
    pid: u32,
    port: u16,
    profile: PathBuf,
    headful: bool,
    active: HashSet<String>,
    idle_since_ms: Option<u64>,
}

const CDP_LAUNCH_ATTEMPTS: usize = 2;

const BROWSER_HOT_IDLE_MS: u64 = 600_000;

fn idle_window_ms() -> u64 {
    std::env::var("GM_BROWSER_IDLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(BROWSER_HOT_IDLE_MS)
}

static BROWSERS: OnceLock<Mutex<HashMap<PathBuf, SharedBrowser>>> = OnceLock::new();
static CLAIMED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static HEARTBEATS: OnceLock<Mutex<HashMap<PathBuf, Arc<AtomicBool>>>> = OnceLock::new();
static HOUSEKEEPING: OnceLock<()> = OnceLock::new();
static AGENT_SEQ: AtomicU64 = AtomicU64::new(0);

fn browsers() -> MutexGuard<'static, HashMap<PathBuf, SharedBrowser>> {
    BROWSERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn claimed() -> MutexGuard<'static, HashSet<PathBuf>> {
    CLAIMED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn heartbeats() -> MutexGuard<'static, HashMap<PathBuf, Arc<AtomicBool>>> {
    HEARTBEATS
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

pub fn session_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("browser-lease").join(SESSION)
}

pub fn owner_file(root: &Path) -> PathBuf {
    session_dir(root).join(OWNER_FILE)
}

pub fn profile_dir(root: &Path) -> PathBuf {
    session_dir(root).join(PROFILE)
}

#[derive(Clone)]
pub struct LeaseHolder {
    pub owner: String,
    pub pid: u32,
    pub browser_pid: u32,
    pub port: u16,
    pub started_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub ttl_ms: u64,
}

impl LeaseHolder {
    fn new(owner: &str, port: u16, browser_pid: u32, ttl_ms: u64) -> Self {
        let now = now_ms();
        Self {
            owner: owner.to_string(),
            pid: std::process::id(),
            browser_pid,
            port,
            started_at_ms: now,
            heartbeat_at_ms: now,
            ttl_ms,
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "owner": self.owner,
            "pid": self.pid,
            "browser_pid": self.browser_pid,
            "port": self.port,
            "startedAt": self.started_at_ms,
            "heartbeatAt": self.heartbeat_at_ms,
            "ttlMs": self.ttl_ms,
        })
    }

    pub fn heartbeat_age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.heartbeat_at_ms)
    }

    pub fn held_for_ms(&self) -> u64 {
        now_ms().saturating_sub(self.started_at_ms)
    }
}

pub fn next_agent_id() -> String {
    format!(
        "crawl-{}-{}",
        std::process::id(),
        AGENT_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn int_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get(key)
                .and_then(Value::as_i64)
                .map(|n| n.max(0) as u64)
        })
        .unwrap_or(0)
}

pub fn read_holder(root: &Path) -> Option<LeaseHolder> {
    let text = std::fs::read_to_string(owner_file(root)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let owner = value.get("owner").and_then(Value::as_str)?;
    let ttl_ms = match int_field(&value, "ttlMs") {
        0 => TTL_MS,
        other => other,
    };
    Some(LeaseHolder {
        owner: owner.to_string(),
        pid: int_field(&value, "pid") as u32,
        browser_pid: int_field(&value, "browser_pid") as u32,
        port: int_field(&value, "port") as u16,
        started_at_ms: int_field(&value, "startedAt"),
        heartbeat_at_ms: int_field(&value, "heartbeatAt"),
        ttl_ms,
    })
}

fn write_holder(root: &Path, holder: &LeaseHolder) -> Result<(), String> {
    let dir = session_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = owner_file(root);
    let tmp = session_dir(root).join(format!("{OWNER_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", holder.to_value()))
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "could not move {} onto {}: {e}",
            tmp.display(),
            path.display()
        )
    })
}

fn effective_ttl(ttl_ms: u64) -> u64 {
    if ttl_ms == 0 {
        TTL_MS
    } else {
        ttl_ms
    }
}

fn file_age_ms(path: &Path) -> u64 {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return u64::MAX;
    };
    let written = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now_ms().saturating_sub(written)
}

pub fn holder_live(root: &Path, ttl_ms: u64) -> bool {
    match read_holder(root) {
        Some(holder) => holder.heartbeat_age_ms() < effective_ttl(ttl_ms),
        None => file_age_ms(&owner_file(root)) < CLAIM_CREATE_GRACE_MS,
    }
}

fn claim_is_ours(root: &Path) -> bool {
    read_holder(root)
        .map(|holder| holder.pid == std::process::id())
        .unwrap_or(false)
}

fn create_claim(root: &Path, holder: &LeaseHolder) -> Result<bool, String> {
    let dir = session_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = owner_file(root);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = file
                .write_all(format!("{}\n", holder.to_value()).as_bytes())
                .and_then(|()| file.sync_all())
            {
                let _ = std::fs::remove_file(&path);
                return Err(format!("could not write {}: {e}", path.display()));
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("could not create {}: {e}", path.display())),
    }
}

fn kill_tree(pid: u32) {
    if pid <= 1 {
        return;
    }
    kill_process_tree(pid);
}

fn evict_stale_holder(root: &Path, prior: Option<&LeaseHolder>) {
    if let Some(holder) = prior {
        kill_tree(holder.browser_pid);
        if holder.pid != std::process::id() {
            kill_tree(holder.pid);
        }
    }
    reap_stale_crawl_browsers(&profile_dir(root));
    let _ = std::fs::remove_file(owner_file(root));
}

fn refresh_heartbeat(root: &Path) -> bool {
    let Some(mut holder) = read_holder(root) else {
        return false;
    };
    if holder.pid != std::process::id() {
        return false;
    }
    holder.heartbeat_at_ms = now_ms();
    write_holder(root, &holder).is_ok()
}

fn start_heartbeat(root: &Path) {
    let key = root.to_path_buf();
    let mut map = heartbeats();
    if map.contains_key(&key) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    map.insert(key.clone(), Arc::clone(&stop));
    drop(map);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
            if !refresh_heartbeat(&key) {
                break;
            }
        }
    });
}

fn stop_heartbeat(root: &Path) {
    if let Some(stop) = heartbeats().remove(root) {
        stop.store(true, Ordering::Relaxed);
    }
}

fn held_by_error(root: &Path, owner: &str, wait_ms: u64) -> String {
    match read_holder(root) {
        Some(holder) => format!(
            "browser-lease: {owner} waited {wait_ms}ms for the accelerated browser session of {} held by {} pid {} for {}ms (since epoch {}ms), so no browser was launched",
            root.display(),
            holder.owner,
            holder.pid,
            holder.held_for_ms(),
            holder.started_at_ms,
        ),
        None => format!(
            "browser-lease: {owner} waited {wait_ms}ms for the accelerated browser session of {} held by an unreadable owner file ({}), so no browser was launched",
            root.display(),
            owner_file(root).display(),
        ),
    }
}

pub fn claim_session(
    root: &Path,
    owner: &str,
    ttl_ms: u64,
    wait_ms: u64,
) -> Result<LeaseHolder, String> {
    let ttl = effective_ttl(ttl_ms);
    let already = { claimed().contains(root) };
    if already {
        if claim_is_ours(root) {
            let holder = read_holder(root).unwrap_or_else(|| LeaseHolder::new(owner, 0, 0, ttl));
            start_heartbeat(root);
            return Ok(holder);
        }
        claimed().remove(root);
        stop_heartbeat(root);
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let holder = LeaseHolder::new(owner, 0, 0, ttl);
        if create_claim(root, &holder)? {
            claimed().insert(root.to_path_buf());
            start_heartbeat(root);
            return Ok(holder);
        }
        if Instant::now() >= deadline {
            return Err(held_by_error(root, owner, wait_ms));
        }
        if !holder_live(root, ttl) {
            let prior = read_holder(root);
            evict_stale_holder(root, prior.as_ref());
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

pub fn release_session(root: &Path) {
    claimed().remove(root);
    stop_heartbeat(root);
    let Some(holder) = read_holder(root) else {
        return;
    };
    if holder.pid != std::process::id() {
        return;
    }
    kill_tree(holder.browser_pid);
    reap_stale_crawl_browsers(&profile_dir(root));
    let _ = std::fs::remove_file(owner_file(root));
}

fn publish_holder(root: &Path, port: u16, chrome_pid: u32) -> Result<(), String> {
    let Some(mut holder) = read_holder(root) else {
        return Ok(());
    };
    if holder.pid != std::process::id() {
        return Ok(());
    }
    holder.port = port;
    holder.browser_pid = chrome_pid;
    holder.heartbeat_at_ms = now_ms();
    write_holder(root, &holder)
}

// A profile is kept across launches so the signed-in state survives. Chrome leaves singleton
// lock files behind when it is killed, and a stale one blocks the next start, so only those go.
fn clear_profile_locks(profile: &Path) {
    for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

fn launch(root: &Path) -> Result<SharedBrowser, String> {
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
    let mut last_failure = String::new();
    for attempt in 1..=CDP_LAUNCH_ATTEMPTS {
        let port = free_local_port()?;
        let profile = profile_dir(root);
        reap_stale_crawl_browsers(&profile);
        clear_profile_locks(&profile);
        std::fs::create_dir_all(&profile)
            .map_err(|e| format!("could not create Chrome profile {}: {e}", profile.display()))?;
        let mut cmd = Command::new(&chrome);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()));
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
                return Err(format!("Chrome launch failed ({}): {e}", chrome.display()));
            }
        };
        let spawn_pid = child.id();
        let mut browser = SharedBrowser {
            child,
            pid: spawn_pid,
            port,
            profile,
            headful: true,
            active: HashSet::new(),
            idle_since_ms: None,
        };
        if endpoint_ready(port, Instant::now() + CDP_READY_DEADLINE, CDP_POLL_INTERVAL, true) {
            browser.pid = crawl_browser_pid(&browser.profile).unwrap_or(spawn_pid);
            return Ok(browser);
        }
        last_failure = format!(
            "attempt {attempt}/{CDP_LAUNCH_ATTEMPTS}: {} ({}) on port {port} did not answer within {}ms: {}",
            "headful Chrome",
            chrome.display(),
            CDP_READY_DEADLINE.as_millis(),
            endpoint_failure(port)
        );
        close_browser(&mut browser);
    }
    Err(format!(
        "{last_failure}. Chrome needs a desktop session to open a window; set GM_BROWSER_CHROME_PATH to a full Chrome otherwise"
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
    match browser.child.try_wait() {
        Ok(Some(_)) => return false,
        Ok(None) => return true,
        Err(_) => {}
    }
    process_exists(browser.pid)
}

// Chromium commits cookies lazily, so a SIGKILL right after a sign-in loses the last of them. The browser is
// asked to exit first and given a few seconds to flush; only then is it killed.
fn close_browser(browser: &mut SharedBrowser) {
    if crate::crawl::request_exit(browser.pid) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(browser.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    kill_tree(browser.pid);
    kill_tree(browser.child.id());
    let _ = browser.child.kill();
    let _ = browser.child.wait();
}

pub fn acquire(root: &Path, agent: &str, headful: bool) -> Result<(u16, u32), String> {
    acquire_session(root, agent, headful, TTL_MS, WAIT_MS)
}

pub fn acquire_session(
    root: &Path,
    agent: &str,
    headful: bool,
    ttl_ms: u64,
    wait_ms: u64,
) -> Result<(u16, u32), String> {
    let reusable = match browsers().get_mut(root) {
        Some(browser) => browser.headful == headful && browser_running(browser),
        None => false,
    };
    if reusable {
        let mut map = browsers();
        let Some(browser) = map.get_mut(root) else {
            return Err("shared Chrome vanished while attaching a lease".to_string());
        };
        browser.active.insert(agent.to_string());
        browser.idle_since_ms = None;
        let port = browser.port;
        let chrome_pid = browser.pid;
        drop(map);
        publish_holder(root, port, chrome_pid)?;
        return Ok((port, chrome_pid));
    }
    if let Some(mut stale) = browsers().remove(root) {
        close_browser(&mut stale);
    }
    claim_session(root, agent, ttl_ms, wait_ms)?;
    let browser = match launch(root) {
        Ok(browser) => browser,
        Err(e) => {
            release_session(root);
            return Err(e);
        }
    };
    let port = browser.port;
    let chrome_pid = browser.pid;
    let mut active = HashSet::new();
    active.insert(agent.to_string());
    browsers().insert(
        root.to_path_buf(),
        SharedBrowser {
            child: browser.child,
            pid: browser.pid,
            port,
            profile: browser.profile,
            headful,
            active,
            idle_since_ms: None,
        },
    );
    publish_holder(root, port, chrome_pid)?;
    Ok((port, chrome_pid))
}

pub fn release(root: &Path, agent: &str) {
    let attached = match browsers().get_mut(root) {
        Some(browser) => {
            browser.active.remove(agent);
            if browser.active.is_empty() {
                browser.idle_since_ms = Some(now_ms());
            }
            true
        }
        None => false,
    };
    if !attached {
        release_session(root);
    }
}

pub fn status(root: &Path) -> Value {
    let mine = {
        let mut map = browsers();
        map.get_mut(root).map(|browser| {
            let mut leases: Vec<String> = browser.active.iter().cloned().collect();
            leases.sort();
            (browser.port, browser.pid, browser_running(browser), leases)
        })
    };
    let holder = read_holder(root);
    let ttl_ms = holder.as_ref().map(|h| h.ttl_ms).unwrap_or(TTL_MS);
    let mut out = json!({
        "root": root.display().to_string(),
        "held": holder.is_some(),
        "owner": holder.as_ref().map(|h| h.owner.clone()),
        "pid": holder.as_ref().map(|h| h.pid),
        "browser_pid": holder.as_ref().map(|h| h.browser_pid),
        "heartbeat_age_ms": holder.as_ref().map(|h| h.heartbeat_age_ms()),
        "ttl_ms": ttl_ms,
        "live": holder_live(root, ttl_ms),
        "lease_file": owner_file(root).display().to_string(),
        "close_deadline_ms": null,
    });
    match (mine, holder) {
        (Some((port, chrome_pid, alive, leases)), _) => {
            out["alive"] = json!(alive);
            out["port"] = json!(port);
            out["chrome_pid"] = json!(chrome_pid);
            out["lease_count"] = json!(leases.len());
            out["leases"] = json!(leases);
        }
        (None, Some(foreign)) => {
            out["alive"] = json!(
                foreign.port > 0
                    && endpoint_ready(foreign.port, Instant::now(), CDP_POLL_INTERVAL, true)
            );
            out["port"] = json!(foreign.port);
            out["chrome_pid"] = json!(foreign.browser_pid);
            out["lease_count"] = json!(0);
            out["leases"] = json!([]);
        }
        (None, None) => {
            out["alive"] = json!(false);
            out["lease_count"] = json!(0);
            out["leases"] = json!([]);
        }
    }
    out
}

pub fn lease_reply(verb: &str, dispatch_root: &Path, body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Req {
        root: Option<PathBuf>,
        agent: Option<String>,
        ttl_ms: Option<u64>,
        wait_ms: Option<u64>,
    }
    let req: Req = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => {
            return json!({"ok": false, "error": format!("{verb} body must be {{root, agent}}: {e}")})
                .to_string()
        }
    };
    let root = req.root.unwrap_or_else(|| dispatch_root.to_path_buf());
    let ttl_ms = req.ttl_ms.unwrap_or(TTL_MS);
    let wait_ms = req.wait_ms.unwrap_or(WAIT_MS);
    let reply = match (verb, req.agent.as_deref()) {
        ("browser_lease_acquire", None) | ("browser_lease_release", None) => {
            json!({"ok": false, "error": format!("{verb} needs an agent")})
        }
        ("browser_lease_acquire", Some(agent)) => {
            match acquire_session(&root, agent, false, ttl_ms, wait_ms) {
                Ok((port, chrome_pid)) => json!({
                    "ok": true,
                    "port": port,
                    "chrome_pid": chrome_pid,
                    "lease_file": owner_file(&root).display().to_string(),
                }),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
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

pub fn housekeeping(_now: Instant) {
    let mut dead: Vec<PathBuf> = Vec::new();
    {
        let mut map = browsers();
        for (root, browser) in map.iter_mut() {
            if !browser_running(browser) {
                dead.push(root.clone());
            }
        }
    }
    for root in dead {
        if let Some(mut browser) = browsers().remove(&root) {
            close_browser(&mut browser);
        }
        release_session(&root);
    }
    let window = idle_window_ms();
    let now = now_ms();
    let expired: Vec<PathBuf> = browsers()
        .iter()
        .filter(|(_, browser)| {
            browser
                .idle_since_ms
                .is_some_and(|since| now.saturating_sub(since) >= window)
        })
        .map(|(root, _)| root.clone())
        .collect();
    for root in expired {
        if let Some(mut browser) = browsers().remove(&root) {
            close_browser(&mut browser);
        }
        release_session(&root);
    }
    let lost: Vec<PathBuf> = {
        let map = claimed();
        map.iter()
            .filter(|root| !claim_is_ours(root.as_path()))
            .map(|root| root.to_path_buf())
            .collect()
    };
    for root in lost {
        if let Some(mut browser) = browsers().remove(&root) {
            close_browser(&mut browser);
        }
        claimed().remove(&root);
        stop_heartbeat(&root);
    }
}

pub fn spawn_housekeeping_timer() {
    if HOUSEKEEPING.set(()).is_err() {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(HOUSEKEEPING_TICK);
        housekeeping(Instant::now());
    });
}

pub fn shutdown_shared_browsers() {
    for (root, mut browser) in browsers().drain() {
        close_browser(&mut browser);
        release_session(&root);
    }
    let still_claimed: Vec<PathBuf> = claimed().drain().collect();
    for root in still_claimed {
        stop_heartbeat(&root);
        release_session(&root);
    }
}
