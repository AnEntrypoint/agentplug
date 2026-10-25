use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::browser::{
    browser_pid, endpoint_failure, endpoint_ready, find_chrome, free_local_port,
    kill_process_tree, process_exists, reap_stale_browsers, CDP_POLL_INTERVAL,
    CDP_READY_DEADLINE,
};
use crate::watcher_log::append_watcher_event;
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
    child: Option<Child>,
    pid: u32,
    port: u16,
    profile: PathBuf,
    headful: bool,
    active: HashSet<String>,
    idle_since_ms: Option<u64>,
    // An attached browser was started by someone else (found through the endpoint); the lease never closes it.
    attached: bool,
}

const CDP_LAUNCH_ATTEMPTS: usize = 2;

// Options a lease caller may give when a browser is opened. Unset fields keep the defaults.
#[derive(Clone, Default)]
pub struct LaunchOptions {
    pub binary_path: Option<PathBuf>,
    pub user_data_dir: Option<PathBuf>,
    pub disable_sandbox: bool,
    // "auto" discovers a running local Chrome; a port or 127.0.0.1/localhost:<port> attaches to that one.
    pub endpoint: Option<String>,
}

const DEFAULT_CDP_PORT: u16 = 9222;
const ATTACH_PROBE: Duration = Duration::from_secs(2);

pub const BROWSER_IDLE_CLOSE_MS: u64 = 15 * 60 * 1_000;

fn idle_window_ms() -> u64 {
    std::env::var("GM_BROWSER_IDLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(BROWSER_IDLE_CLOSE_MS)
}

static BROWSERS: OnceLock<Mutex<HashMap<PathBuf, SharedBrowser>>> = OnceLock::new();
static CLAIMED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static WAITERS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
static HEARTBEATS: OnceLock<Mutex<HashMap<PathBuf, Arc<AtomicBool>>>> = OnceLock::new();
static ATTENDED: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
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

fn waiters() -> MutexGuard<'static, HashMap<PathBuf, usize>> {
    WAITERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct WaitTicket(PathBuf);

impl WaitTicket {
    fn enter(root: &Path) -> Self {
        *waiters().entry(root.to_path_buf()).or_insert(0) += 1;
        if let Some(browser) = browsers().get_mut(root) {
            browser.idle_since_ms = None;
        }
        Self(root.to_path_buf())
    }
}

impl Drop for WaitTicket {
    fn drop(&mut self) {
        let mut map = waiters();
        if let Some(count) = map.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.0);
            }
        }
    }
}

fn has_waiter(root: &Path) -> bool {
    waiters().contains_key(root)
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
        "browser-{}-{}",
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

fn evict_stale_holder(root: &Path, prior: Option<&LeaseHolder>) -> Result<(), String> {
    let (browser_pid, detail) = match prior {
        Some(holder) => (
            holder.browser_pid,
            format!("reason=stale-holder holder_pid={} owner={}", holder.pid, holder.owner),
        ),
        None => (0, "reason=stale-holder holder_pid=unreadable owner=unknown".to_string()),
    };
    log_browser(root, "browser-evicted", browser_pid, &detail);
    if let Some(holder) = prior {
        kill_tree(holder.browser_pid);
        if holder.pid != std::process::id() {
            kill_tree(holder.pid);
        }
    }
    log_browser(root, "browser-reaped", 0, "reason=stale-holder profile-sweep");
    reap_stale_browsers(root)?;
    let _ = std::fs::remove_file(owner_file(root));
    Ok(())
}

fn attended() -> MutexGuard<'static, HashMap<PathBuf, u64>> {
    ATTENDED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// Only a successful acquire renews the lease. Status polls and waiters do not, so an abandoned holder goes stale.
fn attend(root: &Path) {
    attended().insert(root.to_path_buf(), now_ms());
}

// Keeps the lease renewed for as long as the returned guard lives, so a browser step longer than the TTL is not judged dead.
pub struct Attendance {
    stop: Arc<AtomicBool>,
}

impl Drop for Attendance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn attend_while_running(root: &Path) -> Attendance {
    attend(root);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let key = root.to_path_buf();
    std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            attend(&key);
        }
    });
    Attendance { stop }
}

fn attended_within(root: &Path, ttl_ms: u64) -> bool {
    attended()
        .get(root)
        .is_some_and(|at| now_ms().saturating_sub(*at) < ttl_ms)
}

fn refresh_heartbeat(root: &Path) -> bool {
    let Some(mut holder) = read_holder(root) else {
        return false;
    };
    if holder.pid != std::process::id() || !attended_within(root, holder.ttl_ms) {
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
        loop {
            std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
            if stop.load(Ordering::Relaxed) || !refresh_heartbeat(&key) {
                break;
            }
        }
        let mut map = heartbeats();
        if map.get(&key).is_some_and(|current| Arc::ptr_eq(current, &stop)) {
            map.remove(&key);
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
    if claimed().contains(root) && !claim_is_ours(root) {
        claimed().remove(root);
        stop_heartbeat(root);
    }
    if claimed().contains(root) {
        if let Some(holder) = read_holder(root).filter(|holder| holder.owner == owner) {
            attend(root);
            start_heartbeat(root);
            return Ok(holder);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let holder = LeaseHolder::new(owner, 0, 0, ttl);
        if create_claim(root, &holder)? {
            claimed().insert(root.to_path_buf());
            attend(root);
            start_heartbeat(root);
            return Ok(holder);
        }
        if Instant::now() >= deadline {
            return Err(held_by_error(root, owner, wait_ms));
        }
        let prior = read_holder(root);
        let prior_ttl = prior.as_ref().map_or(ttl, |holder| holder.ttl_ms);
        if !holder_live(root, prior_ttl) {
            evict_stale_holder(root, prior.as_ref())?;
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
    log_browser(
        root,
        "browser-released",
        holder.browser_pid,
        &format!("reason=session-release owner={}", holder.owner),
    );
    kill_tree(holder.browser_pid);
    let _ = reap_stale_browsers(root);
    let _ = std::fs::remove_file(owner_file(root));
}

fn release_claim(root: &Path) {
    claimed().remove(root);
    stop_heartbeat(root);
    if claim_is_ours(root) {
        let _ = std::fs::remove_file(owner_file(root));
    }
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

// Chrome for Testing installs live under <home>/.gm/browsers/chrome-for-testing/<dotted.version>/. Newest first.
fn chrome_for_testing_base() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".gm").join("browsers").join("chrome-for-testing"))
}

#[cfg(windows)]
const CFT_RELATIVE: &[&str] = &["chrome-win64/chrome.exe"];
#[cfg(target_os = "macos")]
const CFT_RELATIVE: &[&str] = &[
    "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
];
#[cfg(not(any(windows, target_os = "macos")))]
const CFT_RELATIVE: &[&str] = &["chrome-linux64/chrome"];

fn chrome_for_testing_candidates() -> Vec<PathBuf> {
    let Some(base) = chrome_for_testing_base() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| dotted_version(path.file_name()?.to_str()?).map(|key| (key, path)))
        .collect();
    versions.sort();
    versions.reverse();
    versions
        .into_iter()
        .flat_map(|(_, dir)| CFT_RELATIVE.iter().map(move |rel| dir.join(*rel)))
        .collect()
}

// Lookup order: binaryPath (must exist, nothing else is tried), GM_BROWSER_CHROME_PATH, CHROME_PATH,
// Chrome for Testing under the gm home, then the platform install locations and PATH (browser.rs find_chrome).
fn resolve_binary(explicit: Option<&Path>) -> Result<PathBuf, String> {
    let mut searched: Vec<String> = Vec::new();
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(format!(
            "binaryPath {} is not a file; searched: binaryPath only",
            path.display()
        ));
    }
    for var in ["GM_BROWSER_CHROME_PATH", "CHROME_PATH"] {
        match std::env::var_os(var).map(PathBuf::from) {
            Some(path) if path.is_file() => return Ok(path),
            Some(path) => searched.push(format!("{var}={} (not a file)", path.display())),
            None => searched.push(format!("{var} (unset)")),
        }
    }
    let candidates = chrome_for_testing_candidates();
    match chrome_for_testing_base() {
        Some(base) if candidates.is_empty() => {
            searched.push(format!("Chrome for Testing under {} (no install)", base.display()))
        }
        _ => searched.extend(candidates.iter().map(|path| path.display().to_string())),
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Ok(path);
    }
    if let Some(path) = find_chrome() {
        return Ok(path);
    }
    searched.push(
        "platform install locations, then PATH: chrome, google-chrome, google-chrome-stable, chromium, chromium-browser"
            .to_string(),
    );
    Err(format!(
        "no Chrome found; searched: {}. Install Google Chrome or Chromium, or pass binaryPath",
        searched.join("; ")
    ))
}

fn profile_for(root: &Path, opts: &LaunchOptions) -> PathBuf {
    match &opts.user_data_dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => root.join(dir),
        None => profile_dir(root),
    }
}

// Reads the port Chrome writes to DevToolsActivePort in its profile, then the default CDP port.
fn discover_endpoint(profile: &Path) -> Option<u16> {
    let from_file = std::fs::read_to_string(profile.join("DevToolsActivePort"))
        .ok()
        .and_then(|text| text.lines().next().and_then(|line| line.trim().parse::<u16>().ok()))
        .filter(|port| *port > 0);
    from_file
        .into_iter()
        .chain(std::iter::once(DEFAULT_CDP_PORT))
        .find(|port| endpoint_ready(*port, Instant::now(), CDP_POLL_INTERVAL, true))
}

// Accepts "9222", "127.0.0.1:9222", "localhost:9222" and the same with an http:// prefix. Remote hosts are refused.
fn endpoint_port(endpoint: &str) -> Result<u16, String> {
    let trimmed = endpoint.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let (host, port_text) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => ("127.0.0.1", rest),
    };
    if !matches!(host, "127.0.0.1" | "localhost") {
        return Err(format!(
            "endpoint {endpoint} is not local; only 127.0.0.1 or localhost can be attached"
        ));
    }
    port_text
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("endpoint {endpoint} has no valid port"))
}

// Opens the shared browser: attaches to an endpoint when one is asked for, otherwise launches Chrome.
fn open_browser(
    root: &Path,
    profile: &Path,
    opts: &LaunchOptions,
    headful: bool,
) -> Result<SharedBrowser, String> {
    if let Some(endpoint) = opts.endpoint.as_deref() {
        let port = if endpoint.trim().eq_ignore_ascii_case("auto") {
            discover_endpoint(profile).ok_or_else(|| {
                format!(
                    "no running local Chrome answered on the DevToolsActivePort of {} or on port {DEFAULT_CDP_PORT}; pass endpoint=<port> or leave endpoint unset to launch",
                    profile.display()
                )
            })?
        } else {
            let port = endpoint_port(endpoint)?;
            if !endpoint_ready(port, Instant::now() + ATTACH_PROBE, CDP_POLL_INTERVAL, true) {
                return Err(format!(
                    "endpoint {endpoint} did not answer: {}",
                    endpoint_failure(port)
                ));
            }
            port
        };
        log_browser(root, "browser-discovered", 0, &format!("port={port}"));
        return Ok(SharedBrowser {
            child: None,
            pid: 0,
            port,
            profile: profile.to_path_buf(),
            headful,
            active: HashSet::new(),
            idle_since_ms: None,
            attached: true,
        });
    }
    let binary = resolve_binary(opts.binary_path.as_deref())?;
    launch(root, &binary, profile, opts)
}

fn launch(
    root: &Path,
    chrome: &Path,
    profile: &Path,
    opts: &LaunchOptions,
) -> Result<SharedBrowser, String> {
    if is_headless_shell(chrome) {
        return Err(format!(
            "{} is a headless shell, and the cdp engine needs the full Chrome binary: point binaryPath, GM_BROWSER_CHROME_PATH or CHROME_PATH at chrome.exe",
            chrome.display()
        ));
    }
    // A caller-named profile may belong to a Chrome that is still running, so its locks and component versions are left alone.
    let own_profile = opts.user_data_dir.is_none();
    let mut last_failure = String::new();
    for attempt in 1..=CDP_LAUNCH_ATTEMPTS {
        let port = free_local_port()?;
        reap_stale_browsers(root)?;
        if own_profile {
            clear_profile_locks(profile);
            prune_superseded_component_versions(profile);
        }
        std::fs::create_dir_all(profile)
            .map_err(|e| format!("could not create Chrome profile {}: {e}", profile.display()))?;
        let mut cmd = Command::new(chrome);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--profile-directory=Default");
        if opts.disable_sandbox {
            cmd.args(["--no-sandbox", "--disable-setuid-sandbox"]);
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
                return Err(format!("Chrome launch failed ({}): {e}", chrome.display()));
            }
        };
        let spawn_pid = child.id();
        let mut browser = SharedBrowser {
            child: Some(child),
            pid: spawn_pid,
            port,
            profile: profile.to_path_buf(),
            headful: true,
            active: HashSet::new(),
            idle_since_ms: None,
            attached: false,
        };
        if endpoint_ready(port, Instant::now() + CDP_READY_DEADLINE, CDP_POLL_INTERVAL, true) {
            browser.pid = browser_pid(&browser.profile).unwrap_or(spawn_pid);
            return Ok(browser);
        }
        last_failure = format!(
            "attempt {attempt}/{CDP_LAUNCH_ATTEMPTS}: headful Chrome ({}) on port {port} did not answer within {}ms: {}",
            chrome.display(),
            CDP_READY_DEADLINE.as_millis(),
            endpoint_failure(port)
        );
        close_browser(&mut browser);
    }
    Err(format!(
        "{last_failure}. Chrome needs a desktop session to open a window; set binaryPath or GM_BROWSER_CHROME_PATH to a full Chrome otherwise"
    ))
}

fn is_headless_shell(chrome: &Path) -> bool {
    chrome
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("headless"))
}

fn browser_running(browser: &mut SharedBrowser) -> bool {
    if browser.attached {
        return endpoint_ready(browser.port, Instant::now(), CDP_POLL_INTERVAL, true);
    }
    if endpoint_ready(browser.port, Instant::now(), CDP_POLL_INTERVAL, true) || process_exists(browser.pid) {
        return true;
    }
    // The tracked pid can be a launcher wrapper that exited while Chrome lives on; re-resolve the real pid by profile before judging it gone.
    match browser_pid(&browser.profile) {
        Some(pid) if process_exists(pid) => {
            browser.pid = pid;
            true
        }
        _ => false,
    }
}

fn exit_detail(browser: &mut SharedBrowser) -> String {
    let Some(child) = browser.child.as_mut() else {
        return "adopted from a previous runner, launcher unknown".to_string();
    };
    let launcher = child.id();
    match child.try_wait() {
        Ok(Some(status)) => format!("launcher {launcher}: {status}"),
        Ok(None) => format!("launcher {launcher} still running"),
        Err(e) => format!("launcher status unreadable: {e}"),
    }
}

fn browser_gone(browser: &mut SharedBrowser) -> bool {
    let reaped = match browser.child.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        None => false,
    };
    reaped || !process_exists(browser.pid)
}

pub(crate) fn log_browser(root: &Path, event: &str, pid: u32, detail: &str) {
    append_watcher_event(
        root,
        &format!(
            "{event} root={} pid={pid} runner_pid={} {detail}",
            root.display(),
            std::process::id()
        ),
    );
}

// Chromium commits cookies lazily, so a SIGKILL right after a sign-in loses the last of them. The browser is
// asked to exit first and given a few seconds to flush; only then is it killed.
fn close_browser(browser: &mut SharedBrowser) {
    // An attached browser was not started by the lease; closing the lease only forgets it.
    if browser.attached {
        return;
    }
    if crate::browser::request_exit(browser.pid) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if browser_gone(browser) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    kill_tree(browser.pid);
    if let Some(child) = browser.child.as_mut() {
        kill_tree(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    prune_regenerable_profile_cache(&browser.profile);
}

// Chrome re-downloads these on every launch, so keeping them only costs SSD while the lease is idle.
const REGENERABLE_PROFILE_DIRS: [&str; 2] = ["component_crx_cache", "BrowserMetrics"];

fn prune_regenerable_profile_cache(profile: &Path) {
    for name in REGENERABLE_PROFILE_DIRS {
        let _ = std::fs::remove_dir_all(profile.join(name));
    }
    prune_superseded_component_versions(profile);
}

// Chrome component updates install <profile>/<Component>/<dotted.version>/. Only the highest version of each
// component is kept; the browser is closed when this runs, so no version is in use.
fn prune_superseded_component_versions(profile: &Path) {
    let Ok(components) = std::fs::read_dir(profile) else {
        return;
    };
    for component in components.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()) {
        let Ok(entries) = std::fs::read_dir(&component) else {
            continue;
        };
        let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter_map(|path| dotted_version(path.file_name()?.to_str()?).map(|key| (key, path)))
            .collect();
        if versions.len() < 2 {
            continue;
        }
        versions.sort();
        versions.pop();
        for (_, superseded) in versions {
            let _ = std::fs::remove_dir_all(superseded);
        }
    }
}

fn dotted_version(name: &str) -> Option<Vec<u64>> {
    name.split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()
}

const BROWSER_RECORD_FILE: &str = "browser.json";

fn browser_record_file(root: &Path) -> PathBuf {
    session_dir(root).join(BROWSER_RECORD_FILE)
}

fn write_browser_record(root: &Path, browser: &SharedBrowser) {
    let Some(idle_since) = browser.idle_since_ms else {
        return;
    };
    let dir = session_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("{BROWSER_RECORD_FILE}.{}.tmp", std::process::id()));
    let value = json!({
        "port": browser.port,
        "pid": browser.pid,
        "idle_since_ms": idle_since,
        "headful": browser.headful,
        "profile": browser.profile.display().to_string(),
        "attached": browser.attached,
    });
    if std::fs::write(&tmp, format!("{value}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, browser_record_file(root));
    }
}

fn forget_browser_record(root: &Path) {
    let _ = std::fs::remove_file(browser_record_file(root));
}

fn adopt_idle_browser(
    root: &Path,
    headful: bool,
    wanted: &Path,
    expired_reason: &str,
) -> Option<SharedBrowser> {
    let text = std::fs::read_to_string(browser_record_file(root)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let port = int_field(&value, "port") as u16;
    let pid = int_field(&value, "pid") as u32;
    let idle_since = int_field(&value, "idle_since_ms");
    let recorded_headful = value.get("headful").and_then(Value::as_bool).unwrap_or(false);
    // Records written before the profile field existed were always the lease's own profile.
    let recorded_profile = value
        .get("profile")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| profile_dir(root));
    let attached = value.get("attached").and_then(Value::as_bool).unwrap_or(false);
    if port == 0 || (pid == 0 && !attached) || recorded_headful != headful {
        forget_browser_record(root);
        return None;
    }
    let alive = if attached {
        endpoint_ready(port, Instant::now(), CDP_POLL_INTERVAL, true)
    } else {
        process_exists(pid) && endpoint_ready(port, Instant::now(), CDP_POLL_INTERVAL, true)
    };
    if !alive {
        log_browser(
            root,
            "browser-gone",
            pid,
            &format!("reason=not-alive-at-adoption port={port} idle_since_ms={idle_since}"),
        );
        forget_browser_record(root);
        return None;
    }
    let mut browser = SharedBrowser {
        child: None,
        pid,
        port,
        profile: recorded_profile.clone(),
        headful,
        active: HashSet::new(),
        idle_since_ms: Some(idle_since),
        attached,
    };
    if recorded_profile != wanted {
        close_browser(&mut browser);
        forget_browser_record(root);
        log_browser(
            root,
            "browser-closed",
            pid,
            &format!("reason=profile-mismatch recorded={} wanted={}", recorded_profile.display(), wanted.display()),
        );
        return None;
    }
    if now_ms().saturating_sub(idle_since) >= idle_window_ms() {
        close_browser(&mut browser);
        forget_browser_record(root);
        log_browser(root, "browser-idle-close", pid, expired_reason);
        return None;
    }
    log_browser(
        root,
        "browser-adopted",
        pid,
        &format!("port={port} idle_since_ms={idle_since}"),
    );
    Some(browser)
}

// A runner handoff keeps an idle browser by writing its record; the new process must sweep it, since housekeeping
// walks only the in-memory map and that map is filled by an acquire alone.
pub fn adopt_kept_browsers(roots: &[PathBuf]) {
    for root in roots {
        if browsers().contains_key(root) || claimed().contains(root) || has_waiter(root) {
            continue;
        }
        let Some(value) = std::fs::read_to_string(browser_record_file(root))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            continue;
        };
        let headful = value.get("headful").and_then(Value::as_bool).unwrap_or(false);
        let profile = value
            .get("profile")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| profile_dir(root));
        let expired = "reason=expired-at-sweep-adoption";
        if let Some(browser) = adopt_idle_browser(root, headful, &profile, expired) {
            browsers().insert(root.clone(), browser);
        }
    }
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
    acquire_with(root, agent, &LaunchOptions::default(), headful, ttl_ms, wait_ms)
}

pub fn acquire_with(
    root: &Path,
    agent: &str,
    opts: &LaunchOptions,
    headful: bool,
    ttl_ms: u64,
    wait_ms: u64,
) -> Result<(u16, u32), String> {
    let _waiting = WaitTicket::enter(root);
    claim_session(root, agent, ttl_ms, wait_ms)?;
    attach_after_claim(root, agent, opts, headful)
}

// Drops the shared browser of a root (and its record) without touching the lease. A closed attached browser is only forgotten.
fn drop_shared_browser(root: &Path, cause: &str) {
    if let Some(mut browser) = browsers().remove(root) {
        let pid = browser.pid;
        let detail = exit_detail(&mut browser);
        close_browser(&mut browser);
        log_browser(root, "browser-closed", pid, &format!("reason={cause} {detail}"));
    }
    forget_browser_record(root);
}

// reset: closes the shared browser and gives up the lease. The next acquire launches a fresh browser on the same profile.
pub fn reset_session(root: &Path, agent: &str, ttl_ms: u64, wait_ms: u64) -> Result<(), String> {
    let _waiting = WaitTicket::enter(root);
    claim_session(root, agent, ttl_ms, wait_ms)?;
    drop_shared_browser(root, "reset");
    release_claim(root);
    Ok(())
}

// reconnect: closes the shared browser under the caller's lease and opens it again, keeping the lease.
pub fn reconnect_session(
    root: &Path,
    agent: &str,
    opts: &LaunchOptions,
    headful: bool,
    ttl_ms: u64,
    wait_ms: u64,
) -> Result<(u16, u32), String> {
    let _waiting = WaitTicket::enter(root);
    claim_session(root, agent, ttl_ms, wait_ms)?;
    drop_shared_browser(root, "reconnect");
    attach_after_claim(root, agent, opts, headful)
}

// Runs once the caller holds the claim: adopts, reuses, or opens the shared browser, then records the agent as active.
fn attach_after_claim(
    root: &Path,
    agent: &str,
    opts: &LaunchOptions,
    headful: bool,
) -> Result<(u16, u32), String> {
    let profile = profile_for(root, opts);
    if !browsers().contains_key(root) {
        if let Some(adopted) = adopt_idle_browser(root, headful, &profile, "reason=expired-before-adoption") {
            browsers().insert(root.to_path_buf(), adopted);
        }
    }
    let reusable = match browsers().get_mut(root) {
        Some(browser) => browser.headful == headful && browser_running(browser),
        None => false,
    };
    if !reusable {
        if let Some(mut stale) = browsers().remove(root) {
            let detail = exit_detail(&mut stale);
            let cause = if stale.headful != headful { "headful-mismatch" } else { "not-running" };
            close_browser(&mut stale);
            forget_browser_record(root);
            log_browser(root, "browser-closed", stale.pid, &format!("reason={cause} {detail}"));
        }
        let browser = match open_browser(root, &profile, opts, headful) {
            Ok(browser) => browser,
            Err(e) => {
                release_claim(root);
                return Err(e);
            }
        };
        let launcher_pid = browser.child.as_ref().map_or(0, Child::id);
        let event = if browser.attached { "browser-attached" } else { "browser-open" };
        log_browser(
            root,
            event,
            browser.pid,
            &format!("port={} launcher_pid={launcher_pid}", browser.port),
        );
        browsers().insert(root.to_path_buf(), SharedBrowser { headful, ..browser });
    }
    forget_browser_record(root);
    let attached = browsers().get_mut(root).map(|browser| {
        browser.active.insert(agent.to_string());
        browser.idle_since_ms = None;
        (browser.port, browser.pid)
    });
    let Some((port, chrome_pid)) = attached else {
        release_claim(root);
        return Err("shared Chrome vanished while attaching a lease".to_string());
    };
    if let Err(e) = publish_holder(root, port, chrome_pid) {
        release(root, agent);
        return Err(e);
    }
    Ok((port, chrome_pid))
}

pub fn release(root: &Path, agent: &str) {
    if let Some(browser) = browsers().get_mut(root) {
        browser.active.remove(agent);
        browser.idle_since_ms = Some(now_ms());
        write_browser_record(root, browser);
    }
    release_claim(root);
}

// Gives up the lease and clears the holder record only when `agent` holds it. A caller that never got the lease
// removes nothing, even when another agent of this process holds it (release_claim matches on pid alone).
pub fn release_owned(root: &Path, agent: &str) {
    if read_holder(root).is_some_and(|holder| holder.owner == agent) {
        release(root, agent);
    }
}

pub fn status(root: &Path) -> Value {
    let mine = {
        let mut map = browsers();
        map.get_mut(root).map(|browser| {
            let mut leases: Vec<String> = browser.active.iter().cloned().collect();
            leases.sort();
            let close_deadline = browser
                .idle_since_ms
                .map(|since| since + idle_window_ms());
            (
                browser.port,
                browser.pid,
                browser_running(browser),
                leases,
                close_deadline,
            )
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
    out["attached"] = json!(browsers().get(root).is_some_and(|browser| browser.attached));
    match (mine, holder) {
        (Some((port, chrome_pid, alive, leases, close_deadline)), _) => {
            out["close_deadline_ms"] = json!(close_deadline);
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

// Every shared browser in this process, across roots: the list verb form of browser_lease_status.
pub fn list_browsers() -> Value {
    let rows: Vec<Value> = {
        let mut map = browsers();
        let collected: Vec<Value> = map
            .iter_mut()
            .map(|(root, browser)| {
                let mut leases: Vec<String> = browser.active.iter().cloned().collect();
                leases.sort();
                json!({
                    "root": root.display().to_string(),
                    "port": browser.port,
                    "chrome_pid": browser.pid,
                    "attached": browser.attached,
                    "headful": browser.headful,
                    "profile": browser.profile.display().to_string(),
                    "alive": browser_running(browser),
                    "lease_count": leases.len(),
                    "leases": leases,
                    "close_deadline_ms": browser.idle_since_ms.map(|since| since + idle_window_ms()),
                })
            })
            .collect();
        collected
    };
    json!({"count": rows.len(), "browsers": rows})
}

pub fn lease_reply(verb: &str, dispatch_root: &Path, body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Req {
        root: Option<PathBuf>,
        agent: Option<String>,
        ttl_ms: Option<u64>,
        wait_ms: Option<u64>,
        // Acquire: open with this Chrome binary; it must exist.
        #[serde(rename = "binaryPath")]
        binary_path: Option<String>,
        // Acquire: use this profile directory instead of the project's own.
        #[serde(rename = "userDataDir")]
        user_data_dir: Option<String>,
        // Acquire: add --no-sandbox for root containers. Only when true.
        #[serde(rename = "disableSandbox")]
        disable_sandbox: Option<bool>,
        // Acquire: "auto", or a local port or 127.0.0.1:<port>, to attach to a running Chrome instead of launching.
        endpoint: Option<String>,
        // Acquire: close the shared browser and open it again under this lease.
        reconnect: Option<bool>,
        // Release: close the shared browser, then give up the lease.
        reset: Option<bool>,
        // Status: list every shared browser of this process.
        list: Option<bool>,
    }
    let req: Req = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => {
            return json!({"ok": false, "error": format!("{verb} body must be {{root, agent}}: {e}")})
                .to_string()
        }
    };
    if req.user_data_dir.as_deref().is_some_and(|dir| dir.trim().is_empty()) {
        return json!({"ok": false, "error": format!("{verb} userDataDir is empty")}).to_string();
    }
    let root = req.root.clone().unwrap_or_else(|| dispatch_root.to_path_buf());
    let ttl_ms = req.ttl_ms.unwrap_or(TTL_MS);
    let wait_ms = req.wait_ms.unwrap_or(WAIT_MS);
    let opts = LaunchOptions {
        binary_path: req.binary_path.as_deref().map(PathBuf::from),
        user_data_dir: req.user_data_dir.as_deref().map(PathBuf::from),
        disable_sandbox: req.disable_sandbox.unwrap_or(false),
        endpoint: req.endpoint.clone(),
    };
    let reply = match (verb, req.agent.as_deref()) {
        ("browser_lease_acquire", None) | ("browser_lease_release", None) => {
            json!({"ok": false, "error": format!("{verb} needs an agent")})
        }
        ("browser_lease_acquire", Some(agent)) => {
            let attempt = if req.reconnect.unwrap_or(false) {
                reconnect_session(&root, agent, &opts, true, ttl_ms, wait_ms)
            } else {
                acquire_with(&root, agent, &opts, true, ttl_ms, wait_ms)
            };
            match attempt {
                Ok((port, chrome_pid)) => json!({
                    "ok": true,
                    "port": port,
                    "chrome_pid": chrome_pid,
                    "attached": browsers().get(&root).is_some_and(|browser| browser.attached),
                    "lease_file": owner_file(&root).display().to_string(),
                }),
                Err(e) => json!({"ok": false, "error": e}),
            }
        }
        ("browser_lease_release", Some(agent)) if req.reset.unwrap_or(false) => {
            match reset_session(&root, agent, ttl_ms, wait_ms) {
                Ok(()) => {
                    let mut state = status(&root);
                    state["reset"] = json!(agent);
                    state["ok"] = json!(true);
                    state
                }
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
        ("browser_lease_status", _) if req.list.unwrap_or(false) => {
            let mut state = list_browsers();
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

enum CloseReason {
    Exited,
    Idle { idle_ms: u64 },
    LeaseLost,
}

impl CloseReason {
    fn label(&self) -> String {
        match self {
            Self::Exited => "browser-exited".to_string(),
            Self::Idle { idle_ms } => format!("browser-idle-close idle_ms={idle_ms}"),
            Self::LeaseLost => "browser-lease-lost".to_string(),
        }
    }
}

pub fn housekeeping(_now: Instant) {
    let window = idle_window_ms();
    let mut closing: Vec<(PathBuf, SharedBrowser, CloseReason)> = Vec::new();
    {
        let mut map = browsers();
        let roots: Vec<PathBuf> = map.keys().cloned().collect();
        for root in roots {
            let leased = claimed().contains(&root) && claim_is_ours(&root);
            if leased || has_waiter(&root) {
                continue;
            }
            let Some(browser) = map.get_mut(&root) else {
                continue;
            };
            let reason = if !browser_running(browser) {
                CloseReason::Exited
            } else {
                match browser.idle_since_ms {
                    Some(since) if now_ms().saturating_sub(since) >= window => CloseReason::Idle {
                        idle_ms: now_ms().saturating_sub(since),
                    },
                    _ => continue,
                }
            };
            if let Some(browser) = map.remove(&root) {
                closing.push((root, browser, reason));
            }
        }
    }
    let lost: Vec<PathBuf> = claimed()
        .iter()
        .filter(|root| !claim_is_ours(root.as_path()))
        .cloned()
        .collect();
    for root in lost {
        claimed().remove(&root);
        stop_heartbeat(&root);
        if let Some(browser) = browsers().remove(&root) {
            closing.push((root, browser, CloseReason::LeaseLost));
        }
    }
    for (root, mut browser, reason) in closing {
        let pid = browser.pid;
        let detail = exit_detail(&mut browser);
        close_browser(&mut browser);
        forget_browser_record(&root);
        release_claim(&root);
        log_browser(&root, &reason.label(), pid, &detail);
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
    let window = idle_window_ms();
    let drained: Vec<(PathBuf, SharedBrowser)> = browsers().drain().collect();
    for (root, mut browser) in drained {
        let pid = browser.pid;
        let busy = !browser.active.is_empty() || claimed().contains(&root) || has_waiter(&root);
        let within_window = browser
            .idle_since_ms
            .is_some_and(|since| now_ms().saturating_sub(since) < window);
        if !busy && within_window {
            write_browser_record(&root, &browser);
            log_browser(&root, "browser-kept", pid, "reason=runner-handoff-idle");
            continue;
        }
        let cause = if busy {
            "reason=runner-shutdown-with-holder"
        } else {
            "reason=idle-expired-at-shutdown"
        };
        let detail = exit_detail(&mut browser);
        close_browser(&mut browser);
        forget_browser_record(&root);
        log_browser(&root, "browser-closed", pid, &format!("{cause} {detail}"));
        release_session(&root);
    }
    let still_claimed: Vec<PathBuf> = claimed().drain().collect();
    for root in still_claimed {
        stop_heartbeat(&root);
        release_session(&root);
    }
}
