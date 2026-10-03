use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::idle_reap::IdleReap;

pub(crate) const CDP_EVAL_JS: &str = include_str!("cdp_eval.js");
const EXTENSION_LOAD_JS: &str = include_str!("extension_load.js");
const DEFAULT_CHROME_IDLE_TTL_SECONDS: u64 = 300;
const DEFAULT_CHROME_MAX_CONCURRENT: u64 = 2;
const DEFAULT_SESSION_OWNER_GONE_IDLE_MS: u64 = 60 * 1000;
const IDLE_REAPER_TICK: Duration = Duration::from_secs(15);
const GLOBAL_ORPHAN_LAUNCH_GRACE: Duration = Duration::from_secs(15);
const LRU_EVICTION_IDLE_FLOOR: Duration = Duration::from_secs(60);

#[derive(serde::Deserialize, Default)]
pub(crate) struct BrowserRuntimeConfig {
    #[serde(default)]
    cdp_poll_timeout_ms: Option<u64>,
    #[serde(default)]
    cdp_poll_interval_ms: Option<u64>,
    #[serde(default)]
    chrome_ready_deadline_ms: Option<u64>,
    #[serde(default)]
    eval_timeout_grace_ms: Option<u64>,
    #[serde(default)]
    headless: Option<bool>,
    #[serde(default)]
    session_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    session_owner_gone_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    load_extension: Option<String>,
    #[serde(default)]
    chrome_extra_args: Option<Vec<Value>>,
    #[serde(default)]
    enable_webgpu: Option<bool>,
    #[serde(default)]
    chrome_idle_ttl_seconds: Option<u64>,
    #[serde(default)]
    chrome_max_concurrent: Option<u64>,
    #[serde(default)]
    gpu: Option<String>,
    #[serde(default)]
    headless_disable_gpu: Option<bool>,
    #[serde(default)]
    uncapped: Option<bool>,
}

type BrowserConfig = BrowserRuntimeConfig;

impl BrowserRuntimeConfig {
    fn load(cwd: &Path) -> Self {
        let path = cwd.join(".gm").join("browser-config.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BrowserRuntimeConfig>(&s).ok())
            .unwrap_or_default()
    }
    fn cdp_poll_timeout(&self) -> Duration { Duration::from_millis(self.cdp_poll_timeout_ms.unwrap_or(1000)) }
    fn cdp_poll_interval(&self) -> Duration { Duration::from_millis(self.cdp_poll_interval_ms.unwrap_or(250)) }
    pub(crate) fn chrome_ready_deadline(&self) -> Duration { Duration::from_millis(self.chrome_ready_deadline_ms.unwrap_or(30_000)) }
    fn eval_timeout_grace(&self) -> u64 { self.eval_timeout_grace_ms.unwrap_or(6000) }
    fn headless(&self) -> bool { self.headless.unwrap_or(false) }
    fn session_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.session_idle_timeout_ms.unwrap_or(30 * 60 * 1000))
    }
    fn session_owner_gone_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.session_owner_gone_idle_timeout_ms.unwrap_or(DEFAULT_SESSION_OWNER_GONE_IDLE_MS))
    }
    pub(crate) fn load_extension(&self) -> Option<&str> {
        self.load_extension.as_deref()
    }
    fn chrome_extra_args(&self) -> &[Value] { self.chrome_extra_args.as_deref().unwrap_or(&[]) }
    fn enable_webgpu(&self) -> bool { self.enable_webgpu.unwrap_or(false) }
    fn headless_disable_gpu(&self) -> bool { self.headless_disable_gpu.unwrap_or(false) }
    fn configured_gpu(&self) -> Option<&str> { self.gpu.as_deref() }
    fn uncapped(&self) -> bool { self.uncapped.unwrap_or(false) }
    fn chrome_idle_ttl(&self) -> Duration {
        Duration::from_secs(self.chrome_idle_ttl_seconds.filter(|s| *s > 0).unwrap_or(DEFAULT_CHROME_IDLE_TTL_SECONDS))
    }
    fn chrome_max_concurrent(&self) -> usize {
        self.chrome_max_concurrent.filter(|n| *n > 0).unwrap_or(DEFAULT_CHROME_MAX_CONCURRENT) as usize
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{cmd}.exe"), format!("{cmd}.cmd"), cmd.to_string()]
    } else {
        vec![cmd.to_string()]
    };
    std::env::split_paths(&path_var).find_map(|p| {
        for n in &names {
            let cand = p.join(n);
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    })
}

fn explicit_chrome_path_override() -> Option<PathBuf> {
    std::env::var_os("GM_BROWSER_CHROME_PATH")
        .or_else(|| std::env::var_os("CHROME_PATH"))
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

fn find_chrome_under_playwright_browsers_path() -> Option<PathBuf> {
    let root = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")?;
    let root = PathBuf::from(root);
    if !root.is_dir() {
        return None;
    }
    let exe_name = if cfg!(windows) { "chrome.exe" } else { "chrome" };
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(exe_name) {
                return Some(path);
            }
        }
    }
    None
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = explicit_chrome_path_override() {
        return Some(p);
    }
    if let Some(p) = find_chrome_under_playwright_browsers_path() {
        return Some(p);
    }
    let candidates = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        let mut v = vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(home).join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
        }
        v
    } else {
        vec![
            PathBuf::from("/usr/bin/google-chrome"),
            PathBuf::from("/usr/bin/chromium"),
            PathBuf::from("/usr/bin/chromium-browser"),
        ]
    };
    for c in candidates {
        if c.exists() {
            return Some(c);
        }
    }
    which("chrome").or_else(|| which("google-chrome")).or_else(|| which("chromium"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(9222)
}

pub(crate) fn free_port_probe() -> u16 { free_port() }

fn cdp_endpoint_ready(endpoint: &str, deadline: Instant, cfg: &BrowserConfig) -> bool {
    while Instant::now() < deadline {
        let url = format!("{}/json/version", endpoint.trim_end_matches('/'));
        if let Ok(resp) = ureq::get(&url).timeout(cfg.cdp_poll_timeout()).call() {
            if let Ok(body) = resp.into_string() {
                if body.contains("webSocketDebuggerUrl") {
                    return true;
                }
            }
        }
        std::thread::sleep(cfg.cdp_poll_interval());
    }
    false
}

fn cdp_ready(port: u16, deadline: Instant, cfg: &BrowserConfig) -> bool {
    cdp_endpoint_ready(&format!("http://127.0.0.1:{port}"), deadline, cfg)
}

pub(crate) fn cdp_ready_probe(port: u16, deadline: Instant, cfg: &BrowserRuntimeConfig) -> bool {
    cdp_ready(port, deadline, cfg)
}

pub(crate) fn cdp_endpoint_ready_probe(endpoint: &str, deadline: Instant, cfg: &BrowserRuntimeConfig) -> bool {
    cdp_endpoint_ready(endpoint, deadline, cfg)
}

const BARE_URL_LINE_SCHEMES: [&str; 5] = ["http://", "https://", "about:blank", "file://", "data:"];

fn strip_url_prefix(body: &str) -> (Option<String>, String, &str) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("url=") {
        if let Some(nl) = rest.find('\n') {
            return (Some(rest[..nl].trim().to_string()), String::new(), &rest[nl + 1..]);
        }
        return (Some(rest.trim().to_string()), String::new(), "");
    }
    if BARE_URL_LINE_SCHEMES.iter().any(|scheme| trimmed.starts_with(scheme)) {
        if let Some(nl) = trimmed.find('\n') {
            return (Some(trimmed[..nl].trim().to_string()), String::new(), &trimmed[nl + 1..]);
        }
        return (Some(trimmed.trim().to_string()), "return {url: location.href};".to_string(), "");
    }
    (None, String::new(), body)
}

#[derive(Clone, Copy, PartialEq)]
enum BrowserMode {
    Default,
    Capture,
    Profile,
    Trace,
    Screenshot,
    Dom,
    Gpu,
}

fn strip_mode_prefix(body: &str) -> (BrowserMode, String, &str) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("capture gl\n") {
        return (BrowserMode::Capture, "gl".to_string(), rest);
    }
    for (prefix, mode) in [
        ("capture\n", BrowserMode::Capture),
        ("profile\n", BrowserMode::Profile),
        ("trace\n", BrowserMode::Trace),
        ("screenshot\n", BrowserMode::Screenshot),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (mode, String::new(), rest);
        }
    }
    if let Some(rest) = trimmed.strip_prefix("screenshot=") {
        let (name, remainder) = match rest.find('\n') {
            Some(nl) => (rest[..nl].trim().to_string(), &rest[nl + 1..]),
            None => (rest.trim().to_string(), ""),
        };
        return (BrowserMode::Screenshot, name, remainder);
    }
    if let Some(rest) = trimmed.strip_prefix("dom=") {
        let (selector, remainder) = match rest.find('\n') {
            Some(nl) => (rest[..nl].trim().to_string(), &rest[nl + 1..]),
            None => (rest.trim().to_string(), ""),
        };
        return (BrowserMode::Dom, selector, remainder);
    }
    (BrowserMode::Default, String::new(), body)
}

fn strip_debug_visibility_prefix(body: &str) -> (Option<bool>, &str) {
    let trimmed = body.trim_start();
    for (prefix, quiet) in [("quiet\n", true), ("debug=off\n", true), ("debug=on\n", false), ("verbose\n", false)] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (Some(quiet), rest);
        }
    }
    (None, body)
}

const QUIET_DEBUG_NOTE: &str = "network, performance and gl detail omitted (quiet is the default); put `capture` as the first body line, or `debug=on`, for the full debug block";
const QUIET_NOTABLE_LIMIT: usize = 5;
const QUIET_TEXT_LIMIT: usize = 200;

fn truncated_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}...")
}

fn console_line_text(entry: &Value) -> String {
    entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|args| args.iter().map(|a| a.as_str().map(str::to_string).unwrap_or_else(|| a.to_string())).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

fn compact_debug(debug: &Value) -> Value {
    let console = debug.get("console").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let console_dropped = debug.get("console_dropped").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut by_type = serde_json::Map::new();
    for entry in &console {
        let kind = entry.get("type").and_then(|v| v.as_str()).unwrap_or("log");
        let seen = by_type.get(kind).and_then(|v| v.as_u64()).unwrap_or(0);
        by_type.insert(kind.to_string(), json!(seen + 1));
    }
    let notable: Vec<Value> = console
        .iter()
        .filter(|entry| matches!(entry.get("type").and_then(|v| v.as_str()), Some("error" | "warning" | "assert")))
        .take(QUIET_NOTABLE_LIMIT)
        .map(|entry| json!({
            "type": entry.get("type").cloned().unwrap_or(Value::Null),
            "text": truncated_text(&console_line_text(entry), QUIET_TEXT_LIMIT),
        }))
        .collect();
    let page_errors = debug.get("pageErrors").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let shown_page_errors: Vec<Value> = page_errors
        .iter()
        .take(QUIET_NOTABLE_LIMIT)
        .map(|entry| json!({
            "text": truncated_text(entry.get("text").and_then(|v| v.as_str()).unwrap_or(""), QUIET_TEXT_LIMIT),
            "url": entry.get("url").cloned().unwrap_or(Value::Null),
            "line": entry.get("line").cloned().unwrap_or(Value::Null),
        }))
        .collect();
    let network = debug.get("network").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let network_dropped = debug.get("network_dropped").and_then(|v| v.as_u64()).unwrap_or(0);
    let requests = network.iter().filter(|e| e.get("phase").and_then(|v| v.as_str()) == Some("request")).count() as u64 + network_dropped;
    let failed: Vec<Value> = network
        .iter()
        .filter(|e| e.get("phase").and_then(|v| v.as_str()) == Some("response"))
        .filter(|e| e.get("status").and_then(|v| v.as_u64()).is_some_and(|status| status >= 400))
        .take(QUIET_NOTABLE_LIMIT)
        .map(|e| json!({
            "status": e.get("status").cloned().unwrap_or(Value::Null),
            "url": truncated_text(e.get("url").and_then(|v| v.as_str()).unwrap_or(""), QUIET_TEXT_LIMIT),
        }))
        .collect();
    json!({
        "console_summary": { "total": console.len() as u64 + console_dropped, "by_type": by_type, "notable": notable },
        "pageErrors": shown_page_errors,
        "pageErrors_total": page_errors.len(),
        "network_summary": { "requests": requests, "failed": failed },
        "note": QUIET_DEBUG_NOTE,
    })
}

fn strip_timeout_prefix(body: &str) -> (Option<u64>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("timeout=") else { return (None, body) };
    let Some(nl) = rest.find('\n') else { return (None, body) };
    let (num_str, remainder) = (&rest[..nl], &rest[nl + 1..]);
    match num_str.trim().parse::<u64>() {
        Ok(ms) => (Some(ms), remainder),
        Err(_) => (None, body),
    }
}

fn strip_session_id_prefix(body: &str) -> (Option<String>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("sessionId=") else { return (None, body) };
    let Some(nl) = rest.find('\n') else { return (None, body) };
    let (id, remainder) = (&rest[..nl], &rest[nl + 1..]);
    let id = id.trim();
    if id.is_empty() { (None, remainder) } else { (Some(id.to_string()), remainder) }
}

fn strip_viewport_width_height_scale_mobile_prefix(body: &str) -> (Option<(u32, u32, f64, bool)>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("viewport=") else { return (None, body) };
    let Some(nl) = rest.find('\n') else { return (None, body) };
    let (spec, remainder) = (&rest[..nl], &rest[nl + 1..]);
    let (dims_and_scale, mobile) = match spec.strip_suffix("!mobile") {
        Some(rest) => (rest, true),
        None => (spec, false),
    };
    let (dims, scale) = match dims_and_scale.split_once('@') {
        Some((d, s)) => (d, s.trim().parse::<f64>().unwrap_or(1.0)),
        None => (dims_and_scale, 1.0),
    };
    let Some((w, h)) = dims.trim().split_once('x') else { return (None, body) };
    match (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
        (Ok(width), Ok(height)) if width > 0 && height > 0 => {
            (Some((width, height, scale, mobile)), remainder)
        }
        _ => (None, body),
    }
}

fn browser_profiles_dir(cwd: &Path) -> PathBuf {
    cwd.join(".gm").join("browser-profiles")
}

fn browser_chrome_profile_dir(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(".gm").join(format!("browser-chrome-profile-{}", sanitize(session_id)))
}

struct BrowserSession {
    cwd: PathBuf,
    session_id: String,
    owner_gm_session: Option<String>,
    child: Option<Child>,
    pid: u32,
    port: u16,
    cdp_endpoint: String,
    last_used: Instant,
    target_id: Option<String>,
    owns_process: bool,
    engine: crate::browser_engine::Engine,
    idle_reap: Option<IdleReap>,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, BrowserSession>>> = OnceLock::new();

fn sessions_map() -> &'static Mutex<HashMap<String, BrowserSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_key(cwd: &Path, session_id: &str) -> String {
    format!("{}\u{0}{}", cwd.display(), session_id)
}

static SESSION_LIFECYCLE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

fn session_lifecycle_lock_for_key(key: &str) -> Arc<Mutex<()>> {
    let mut locks = SESSION_LIFECYCLE_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    locks.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

fn session_is_alive(session: &mut BrowserSession) -> bool {
    if !session.owns_process {
        return session_cdp_endpoint_responds(&session.cdp_endpoint);
    }
    match session.child.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => pid_is_alive(session.pid),
    }
}

fn session_cdp_endpoint_responds(endpoint: &str) -> bool {
    let url = format!("{}/json/version", endpoint.trim_end_matches('/'));
    match ureq::get(&url).timeout(std::time::Duration::from_millis(1500)).call() {
        Ok(resp) => resp.into_string().map(|b| b.contains("webSocketDebuggerUrl")).unwrap_or(false),
        Err(_) => false,
    }
}

fn kill_session(mut session: BrowserSession) {
    if !session.owns_process {
        return;
    }
    let launched_by_this_process = session.child.is_some();
    if let Some(mut child) = session.child.take() {
        kill_pid(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
    let profile_dir = browser_chrome_profile_dir(&session.cwd, &session.session_id);
    let killed_on_profile = kill_chrome_processes_left_on_profile(&profile_dir);
    if !launched_by_this_process && killed_on_profile == 0 {
        eprintln!(
            "[agentplug browser] session {} (adopted pid {}) matched no chrome whose --user-data-dir is {} -- nothing killed; an adopted pid is never killed by number alone because it may have been reused by an unrelated process",
            session.session_id,
            session.pid,
            profile_dir.display()
        );
    }
    let _ = std::fs::remove_file(pid_sidecar_path(&profile_dir));
    let _ = std::fs::remove_file(port_sidecar_path(&profile_dir));
    let _ = std::fs::remove_file(session_id_sidecar_path(&profile_dir));
    let _ = std::fs::remove_file(owner_gm_session_sidecar_path(&profile_dir));
    let _ = std::fs::remove_file(last_used_sidecar_path(&profile_dir));
}

fn kill_chrome_processes_left_on_profile(profile_dir: &Path) -> usize {
    let pids = pids_of_chrome_processes_using_profile_dir(&list_chrome_processes(), profile_dir);
    for pid in &pids {
        kill_pid(*pid);
    }
    pids.len()
}

fn pid_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.pid")
}

fn port_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.port")
}

fn session_id_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.session-id")
}

fn owner_gm_session_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.owner-gm-session")
}

fn record_owner_gm_session(cwd: &Path, session_id: &str, owner_gm_session: Option<&str>) {
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let sidecar = owner_gm_session_sidecar_path(&profile_dir);
    match owner_gm_session {
        Some(owner) if profile_dir.is_dir() => {
            let _ = std::fs::write(sidecar, owner);
        }
        _ => {
            let _ = std::fs::remove_file(sidecar);
        }
    }
}

fn target_id_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.target-id")
}

fn write_target_id_sidecar(profile_dir: &Path, target_id: &str) {
    let _ = std::fs::write(target_id_sidecar_path(profile_dir), target_id);
}

fn last_used_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.last-used")
}

fn touch_last_used(cwd: &Path, session_id: &str, key: &str) {
    if let Some(session) = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).get_mut(key) {
        session.last_used = Instant::now();
    }
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    if profile_dir.is_dir() {
        let _ = std::fs::write(last_used_sidecar_path(&profile_dir), unix_ms().to_string());
    }
}

fn last_used_recorded_in_sidecars(profile_dir: &Path) -> Instant {
    [last_used_sidecar_path(profile_dir), target_id_sidecar_path(profile_dir), pid_sidecar_path(profile_dir)]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok()?.elapsed().ok())
        .min()
        .and_then(|idle| Instant::now().checked_sub(idle))
        .unwrap_or_else(Instant::now)
}

fn write_session_sidecars(profile_dir: &Path, pid: u32, port: u16, session_id: &str) {
    let _ = std::fs::write(pid_sidecar_path(profile_dir), pid.to_string());
    let _ = std::fs::write(port_sidecar_path(profile_dir), port.to_string());
    let _ = std::fs::write(session_id_sidecar_path(profile_dir), session_id);
}

fn try_adopt_orphaned_session(cwd: &Path, session_id_hint: Option<&str>, profile_dir: &Path) -> Option<String> {
    let session_id = std::fs::read_to_string(session_id_sidecar_path(profile_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| session_id_hint.map(|s| s.to_string()))
        .or_else(|| {
            profile_dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("browser-chrome-profile-"))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })?;
    let pid: u32 = std::fs::read_to_string(pid_sidecar_path(profile_dir)).ok()?.trim().parse().ok()?;
    let port: u16 = std::fs::read_to_string(port_sidecar_path(profile_dir)).ok()?.trim().parse().ok()?;
    if !pid_is_alive(pid) || !session_cdp_endpoint_responds(&format!("http://127.0.0.1:{port}")) {
        return None;
    }
    let target_id = std::fs::read_to_string(target_id_sidecar_path(profile_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let owner_gm_session = std::fs::read_to_string(owner_gm_session_sidecar_path(profile_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let key = session_key(cwd, &session_id);
    {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get_mut(&key) {
            if session_is_alive(existing) {
                return Some(session_id);
            }
            map.remove(&key);
        }
        map.insert(
            key,
            BrowserSession {
                cwd: cwd.to_path_buf(),
                session_id: session_id.clone(),
                owner_gm_session,
                child: None,
                pid,
                port,
                cdp_endpoint: format!("http://127.0.0.1:{port}"),
                last_used: last_used_recorded_in_sidecars(profile_dir),
                target_id,
                owns_process: true,
                engine: crate::browser_engine::Engine::Chrome,
                idle_reap: crate::idle_reap::recorded(profile_dir),
            },
        );
    }
    eprintln!(
        "[agentplug browser] adopted OS-orphaned chrome pid={} port={} as session '{}' for {} -- a daemon recycle no longer costs a browser session its chrome/page state",
        pid, port, session_id, cwd.display()
    );
    Some(session_id)
}

#[cfg(windows)]
const ORPHAN_SCAN_SUBPROCESS_TIMEOUT_MS: u64 = 30_000;

#[cfg(windows)]
fn run_bounded_capturing_stdout(cmd: &mut Command) -> Option<Vec<u8>> {
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().ok()?;
    match child.wait_timeout(Duration::from_millis(ORPHAN_SCAN_SUBPROCESS_TIMEOUT_MS)) {
        Ok(Some(_)) => {
            let mut stdout = Vec::new();
            if let Some(mut o) = child.stdout.take() {
                let _ = std::io::Read::read_to_end(&mut o, &mut stdout);
            }
            Some(stdout)
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    crate::windowless::apply_windowless(&mut cmd);
    match run_bounded_capturing_stdout(&mut cmd) {
        Some(stdout) => {
            let s = String::from_utf8_lossy(&stdout);
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        None => true,
    }
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

const DAEMON_STATUS_STALE_MS: u64 = 20_000;

fn this_process_is_the_registered_daemon() -> bool {
    let status_path = crate::install::install_dir().join("daemon-status.json");
    let Ok(raw) = std::fs::read_to_string(&status_path) else { return true };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return true };
    let Some(ts) = v.get("ts").and_then(|t| t.as_u64()) else { return true };
    if unix_ms().saturating_sub(ts as u128) >= DAEMON_STATUS_STALE_MS as u128 {
        return true;
    }
    let Some(recorded_pid) = v.get("pid").and_then(|p| p.as_u64()) else { return true };
    recorded_pid == std::process::id() as u64
}

fn reap_os_orphans(cwd: &Path) {
    if !this_process_is_the_registered_daemon() {
        eprintln!(
            "[agentplug browser] skipping OS-orphan reap for {} -- a different process is the fresh registered daemon, this process cannot safely judge liveness of sessions it does not own",
            cwd.display()
        );
        return;
    }
    let dir = browser_profiles_root_for_orphan_scan(cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let claimed_dirs: std::collections::HashSet<PathBuf> = {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .filter(|s| s.cwd == cwd)
            .map(|s| browser_chrome_profile_dir(&s.cwd, &s.session_id))
            .collect()
    };
    let mut chrome_processes_this_pass: Option<Vec<(u32, String)>> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with("browser-chrome-profile-") {
            continue;
        }
        if claimed_dirs.contains(&path) {
            continue;
        }
        let sidecar = pid_sidecar_path(&path);
        const ORPHAN_REAP_GRACE: Duration = Duration::from_secs(15);
        if let Ok(meta) = std::fs::metadata(&sidecar) {
            if let Ok(age) = meta.modified().and_then(|m| m.elapsed().map_err(|e| std::io::Error::other(e))) {
                if age < ORPHAN_REAP_GRACE {
                    continue;
                }
            }
        }
        if try_adopt_orphaned_session(cwd, None, &path).is_some() {
            continue;
        }
        if chrome_singleton_lock_present(&path) {
            let processes = chrome_processes_this_pass.get_or_insert_with(list_chrome_processes);
            if let Some(owner) = live_foreign_owner_of_profile_chrome(processes, &path) {
                eprintln!(
                    "[agentplug browser] leaving chrome on profile {} alone -- its root process was launched by pid {owner}, which is alive and is not this daemon",
                    path.display()
                );
                continue;
            }
            for pid in pids_of_chrome_processes_using_profile_dir(processes, &path) {
                eprintln!(
                    "[agentplug browser] reaping OS-orphaned chrome pid={} (profile {}, no owning session in this process -- crash/hard-exit orphan, not adoptable)",
                    pid,
                    path.display()
                );
                kill_pid(pid);
            }
        }
        let _ = std::fs::remove_file(&sidecar);
    }
}

fn browser_profiles_root_for_orphan_scan(cwd: &Path) -> PathBuf {
    cwd.join(".gm")
}

fn session_liveness_recheck(port: u16, cdp_endpoint: &str, browser_cfg: &BrowserConfig) -> bool {
    let Some(node) = which("node") else { return false };
    let tmp = std::env::temp_dir();
    let stamp = format!("{}-livecheck-{}", std::process::id(), unix_ms());
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    if std::fs::write(&helper_path, CDP_EVAL_JS.as_bytes()).is_err() {
        return false;
    }
    if std::fs::write(&script_path, b"return 1+1;").is_err() {
        cleanup(&[&helper_path, &script_path]);
        return false;
    }
    let recheck_timeout_ms: u64 = 15000;
    let cfg = json!({
        "port": port,
        "cdpEndpoint": cdp_endpoint,
        "startUrl": Value::Null,
        "scriptFile": script_path.to_string_lossy(),
        "resultFile": result_path.to_string_lossy(),
        "timeoutMs": recheck_timeout_ms,
        "mode": "default",
        "artifactFile": Value::Null,
    })
    .to_string();
    let mut spawn_cmd = Command::new(&node);
    spawn_cmd.arg(&helper_path)
        .arg(&cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        crate::windowless::apply_windowless(&mut spawn_cmd);
    }
    let spawn = spawn_cmd.spawn();
    let alive = match spawn {
        Ok(mut child) => {
            let grace = Duration::from_millis(recheck_timeout_ms + browser_cfg.eval_timeout_grace());
            match child.wait_timeout(grace) {
                Ok(Some(status)) if status.success() => {
                    let v: Option<Value> = std::fs::read_to_string(&result_path)
                        .ok()
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
                    matches!(v, Some(v) if v == json!(2))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    false
                }
            }
        }
        Err(_) => false,
    };
    cleanup(&[&helper_path, &script_path, &result_path]);
    alive
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/F", "/T"]);
    crate::windowless::apply_windowless(&mut cmd);
    let _ = cmd.output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill").args(["-9", "--", &format!("-{pid}")]).output();
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

#[cfg(windows)]
const WMI_CIRCUIT_BREAKER_COOLDOWN_MS: u64 = 300_000;

#[cfg(windows)]
static WMI_LAST_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
fn wmi_circuit_breaker_open() -> bool {
    let last = WMI_LAST_TIMEOUT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(last) < WMI_CIRCUIT_BREAKER_COOLDOWN_MS
}

#[cfg(windows)]
fn list_chrome_processes() -> Vec<(u32, String)> {
    if wmi_circuit_breaker_open() {
        return Vec::new();
    }
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | ForEach-Object { \"$($_.ProcessId)|$($_.CommandLine)\" }",
    ]);
    crate::windowless::apply_windowless(&mut cmd);
    let Some(stdout) = run_bounded_capturing_stdout(&mut cmd) else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        WMI_LAST_TIMEOUT_MS.store(now, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[agentplug browser] list_chrome_processes: Get-CimInstance (WMI) did not answer within {ORPHAN_SCAN_SUBPROCESS_TIMEOUT_MS}ms -- killed the stalled powershell.exe and returning empty. Circuit breaker opened for {WMI_CIRCUIT_BREAKER_COOLDOWN_MS}ms: further orphan-reap passes in that window skip the WMI call entirely instead of re-paying the same timeout every loop iteration and starving real project dispatch."
        );
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&stdout).into_owned();
    text.lines()
        .filter_map(|line| {
            let (pid_str, cmdline) = line.split_once('|')?;
            Some((pid_str.trim().parse::<u32>().ok()?, cmdline.to_string()))
        })
        .collect()
}

#[cfg(not(windows))]
fn list_chrome_processes() -> Vec<(u32, String)> {
    let output = Command::new("ps").args(["-eo", "pid,args"]).output();
    let Ok(o) = output else { return Vec::new() };
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let (pid_str, rest) = trimmed.split_once(char::is_whitespace)?;
            if !rest.contains("chrome") && !rest.contains("chromium") {
                return None;
            }
            Some((pid_str.trim().parse::<u32>().ok()?, rest.to_string()))
        })
        .collect()
}

#[cfg(windows)]
fn parent_pid_of(pid: u32) -> Option<u32> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\").ParentProcessId"),
    ]);
    crate::windowless::apply_windowless(&mut cmd);
    String::from_utf8_lossy(&run_bounded_capturing_stdout(&mut cmd)?).trim().parse().ok()
}

#[cfg(not(windows))]
fn parent_pid_of(pid: u32) -> Option<u32> {
    let output = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn chrome_singleton_lock_present(profile_dir: &Path) -> bool {
    let lock_name = if cfg!(windows) { "lockfile" } else { "SingletonLock" };
    std::fs::symlink_metadata(profile_dir.join(lock_name)).is_ok()
}

fn pids_of_chrome_processes_using_profile_dir(processes: &[(u32, String)], profile_dir: &Path) -> Vec<u32> {
    let wanted = profile_dir_key(&profile_dir.to_string_lossy());
    processes
        .iter()
        .filter(|(_, cmdline)| cmdline_flag_value(cmdline, "--user-data-dir=").is_some_and(|dir| profile_dir_key(&dir) == wanted))
        .map(|(pid, _)| *pid)
        .collect()
}

fn live_foreign_owner_of_profile_chrome(processes: &[(u32, String)], profile_dir: &Path) -> Option<u32> {
    let wanted = profile_dir_key(&profile_dir.to_string_lossy());
    processes
        .iter()
        .filter(|(_, cmdline)| is_gm_launched_root_chrome_process(cmdline))
        .filter(|(_, cmdline)| cmdline_flag_value(cmdline, "--user-data-dir=").is_some_and(|dir| profile_dir_key(&dir) == wanted))
        .filter_map(|(pid, _)| parent_pid_of(*pid))
        .find(|parent| *parent != std::process::id() && pid_is_alive(*parent))
}

fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

pub fn canonical_project_root(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(strip_windows_verbatim_prefix(&resolved.to_string_lossy()))
}

pub fn project_root(path: &Path) -> PathBuf {
    let canonical = canonical_project_root(path);
    match std::process::Command::new("git").arg("-C").arg(&canonical).args(["rev-parse", "--show-toplevel"]).output() {
        Ok(out) if out.status.success() => {
            let top = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
            if top.is_empty() { canonical } else { canonical_project_root(Path::new(&top)) }
        }
        _ => canonical,
    }
}

fn profile_dir_key(path: &str) -> String {
    let unified = strip_windows_verbatim_prefix(path).replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    if cfg!(windows) { trimmed.to_lowercase() } else { trimmed.to_string() }
}

fn is_gm_owned_chrome_profile_dir(profile_key: &str) -> bool {
    let mut segments = profile_key.rsplit('/');
    let leaf = segments.next().unwrap_or("");
    let parent = segments.next().unwrap_or("");
    parent == ".gm" && leaf.len() > "browser-chrome-profile-".len() && leaf.starts_with("browser-chrome-profile-")
}

fn cmdline_flag_value(cmdline: &str, flag: &str) -> Option<String> {
    let start = cmdline.find(flag)?;
    let whole_argument_quoted = cmdline[..start].ends_with('"');
    let rest = &cmdline[start + flag.len()..];
    let value = if whole_argument_quoted {
        rest.split('"').next()
    } else if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next()
    } else {
        rest.split_whitespace().next()
    }?;
    Some(value.to_string()).filter(|v| !v.is_empty())
}

fn is_gm_launched_root_chrome_process(cmdline: &str) -> bool {
    let has_debug_port = cmdline.contains("--remote-debugging-port=");
    let has_user_data_dir = cmdline.contains("--user-data-dir=");
    let is_child_process = cmdline.contains("--type=");
    has_debug_port && has_user_data_dir && !is_child_process
}

fn profile_launched_within(profile_dir: &Path, grace: Duration) -> bool {
    [pid_sidecar_path(profile_dir), last_used_sidecar_path(profile_dir)]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok()?.elapsed().ok())
        .any(|age| age < grace)
}

fn profile_is_under_served_root(profile_key: &str, served_gm_dir_keys: &std::collections::HashSet<String>) -> bool {
    profile_key.rsplit_once('/').is_some_and(|(gm_dir, _)| served_gm_dir_keys.contains(gm_dir))
}

fn reap_globally_orphaned_gm_chromes(served_roots: &[PathBuf]) {
    if !this_process_is_the_registered_daemon() {
        eprintln!(
            "[agentplug browser] skipping global gm-chrome orphan reap -- a different process is the fresh registered daemon, this process cannot safely judge liveness of sessions it does not own"
        );
        return;
    }
    let claimed_profile_dirs: std::collections::HashSet<String> = {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .map(|s| profile_dir_key(&browser_chrome_profile_dir(&s.cwd, &s.session_id).to_string_lossy()))
            .collect()
    };
    let served_gm_dir_keys: std::collections::HashSet<String> =
        served_roots.iter().map(|root| profile_dir_key(&browser_profiles_root_for_orphan_scan(root).to_string_lossy())).collect();
    if served_gm_dir_keys.is_empty() {
        return;
    }
    for (pid, cmdline) in list_chrome_processes() {
        if !is_gm_launched_root_chrome_process(&cmdline) {
            continue;
        }
        let Some(profile_dir) = cmdline_flag_value(&cmdline, "--user-data-dir=") else { continue };
        let profile_key = profile_dir_key(&profile_dir);
        if !is_gm_owned_chrome_profile_dir(&profile_key) || !profile_is_under_served_root(&profile_key, &served_gm_dir_keys) {
            continue;
        }
        if claimed_profile_dirs.contains(&profile_key) {
            continue;
        }
        if !pid_is_alive(pid) || profile_launched_within(Path::new(&profile_dir), GLOBAL_ORPHAN_LAUNCH_GRACE) {
            continue;
        }
        if parent_pid_of(pid).is_some_and(|parent| parent != std::process::id() && pid_is_alive(parent)) {
            continue;
        }
        eprintln!(
            "[agentplug browser] reaping globally-orphaned gm chrome pid={} (profile {} not tracked by any live session in this process)",
            pid, profile_dir
        );
        kill_pid(pid);
        kill_chrome_processes_left_on_profile(Path::new(&profile_dir));
    }
}

pub fn close_all_sessions() {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        if let Some(session) = map.remove(&k) {
            eprintln!(
                "[agentplug browser] detaching session {} (pid {}) for handoff/shutdown -- chrome keeps running as an adoptable orphan",
                session.session_id, session.pid
            );
            drop(session);
        }
    }
}

pub fn reap_idle_sessions_and_os_orphans_across_every_known_project_root(roots: &[std::path::PathBuf]) {
    let mut canonical_roots: Vec<PathBuf> = roots.iter().map(|r| canonical_project_root(r)).collect();
    canonical_roots.sort();
    canonical_roots.dedup();
    for root in &canonical_roots {
        let cfg = BrowserConfig::load(root);
        reap_idle_sessions(root, &cfg);
        reap_os_orphans(root);
    }
    reap_sessions_for_deregistered_roots(&canonical_roots);
    reap_globally_orphaned_gm_chromes(&canonical_roots);
}

fn reap_sessions_for_deregistered_roots(roots: &[std::path::PathBuf]) {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let dead_keys: Vec<String> = map
        .iter()
        .filter(|(_, s)| !roots.iter().any(|r| r == &s.cwd))
        .map(|(k, _)| k.clone())
        .collect();
    let mut reaped = Vec::new();
    for k in dead_keys {
        if let Some(session) = map.remove(&k) {
            eprintln!(
                "[agentplug browser] reaping session {} for deregistered root {} (no longer in known_roots)",
                session.session_id,
                session.cwd.display()
            );
            reaped.push(session);
        }
    }
    drop(map);
    for session in reaped {
        kill_session(session);
    }
    evict_session_lifecycle_locks_with_no_active_holder();
}

fn owner_gm_session_is_stale(owner_gm_session: Option<&str>, threshold: Duration) -> bool {
    let Some(owner) = owner_gm_session else { return false };
    match crate::dispatch_origin::session_activity_elapsed(owner) {
        Some(elapsed) => elapsed >= threshold,
        None => process_uptime() >= threshold,
    }
}

fn process_uptime() -> Duration {
    static PROCESS_STARTED: OnceLock<Instant> = OnceLock::new();
    PROCESS_STARTED.get_or_init(Instant::now).elapsed()
}

fn effective_idle_timeout(session: &BrowserSession, cfg: &BrowserConfig) -> Option<Duration> {
    match session.idle_reap {
        Some(IdleReap::Never) => return None,
        Some(IdleReap::After(explicit)) => return Some(explicit),
        None => {}
    }
    let long_ceiling = cfg.session_idle_timeout();
    let short_ceiling = cfg.session_owner_gone_idle_timeout();
    let session_ceiling = if owner_gm_session_is_stale(session.owner_gm_session.as_deref(), short_ceiling) {
        short_ceiling.min(long_ceiling)
    } else {
        long_ceiling
    };
    Some(if session.owns_process { session_ceiling.min(cfg.chrome_idle_ttl()) } else { session_ceiling })
}

fn dispatch_in_flight(key: &str) -> bool {
    let lock = session_lifecycle_lock_for_key(key);
    let blocked = matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock));
    blocked
}

fn reap_idle_sessions(cwd: &Path, cfg: &BrowserConfig) {
    process_uptime();
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let dead_keys: Vec<String> = map
        .iter()
        .filter(|(k, s)| {
            s.cwd == cwd
                && effective_idle_timeout(s, cfg).is_some_and(|timeout| s.last_used.elapsed() > timeout)
                && !dispatch_in_flight(k)
        })
        .map(|(k, _)| k.clone())
        .collect();
    let mut reaped = Vec::new();
    for k in dead_keys {
        if let Some(session) = map.remove(&k) {
            let timeout_ms = effective_idle_timeout(&session, cfg).map(|t| t.as_millis()).unwrap_or_default();
            let owner_gone = owner_gm_session_is_stale(session.owner_gm_session.as_deref(), cfg.session_owner_gone_idle_timeout());
            eprintln!(
                "[agentplug browser] reaping idle session {} (idle {}ms > {}ms, idle_reap={}, owner_gm_session={:?}, owner_gone={})",
                session.session_id,
                session.last_used.elapsed().as_millis(),
                timeout_ms,
                IdleReap::report(session.idle_reap),
                session.owner_gm_session,
                owner_gone
            );
            reaped.push(session);
        }
    }
    drop(map);
    for session in reaped {
        kill_session(session);
    }
    evict_session_lifecycle_locks_with_no_active_holder();
}

static IDLE_REAPER_STARTED: OnceLock<()> = OnceLock::new();

fn ensure_idle_reaper_running() {
    if IDLE_REAPER_STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("agentplug-browser-idle-reaper".to_string()).spawn(|| loop {
        std::thread::sleep(IDLE_REAPER_TICK);
        reap_idle_sessions_of_every_tracked_project();
    });
}

fn reap_idle_sessions_of_every_tracked_project() {
    let mut project_roots: Vec<PathBuf> = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).values().map(|s| s.cwd.clone()).collect();
    project_roots.sort();
    project_roots.dedup();
    for root in project_roots {
        reap_idle_sessions(&root, &BrowserConfig::load(&root));
    }
}

struct OwnedChrome {
    key: String,
    pid: u32,
    project: PathBuf,
    session_id: String,
    idle: Duration,
    keep_alive: bool,
}

fn owned_chrome_sessions() -> Vec<OwnedChrome> {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let is_owned_chrome = |s: &BrowserSession| s.owns_process && s.engine == crate::browser_engine::Engine::Chrome;
    let exited: Vec<String> = map
        .iter_mut()
        .filter_map(|(k, s)| (is_owned_chrome(s) && s.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))).then(|| k.clone()))
        .collect();
    for k in exited {
        map.remove(&k);
    }
    map.iter()
        .filter(|(_, s)| is_owned_chrome(s))
        .map(|(k, s)| OwnedChrome {
            key: k.clone(),
            pid: s.pid,
            project: s.cwd.clone(),
            session_id: s.session_id.clone(),
            idle: s.last_used.elapsed(),
            keep_alive: s.idle_reap == Some(IdleReap::Never),
        })
        .collect()
}

static CHROME_CAP_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);
static PENDING_CHROME_LAUNCHES: AtomicUsize = AtomicUsize::new(0);
static CHROME_ADMISSION: Mutex<()> = Mutex::new(());

fn effective_chrome_cap(cfg: &BrowserConfig) -> usize {
    let this_project = cfg.chrome_max_concurrent();
    CHROME_CAP_HIGH_WATER.fetch_max(this_project, Ordering::Relaxed).max(this_project)
}

struct ChromeLaunchReservation;

impl Drop for ChromeLaunchReservation {
    fn drop(&mut self) {
        PENDING_CHROME_LAUNCHES.fetch_sub(1, Ordering::Relaxed);
    }
}

fn chrome_cap_exceeded_message(cap: usize, live: &[OwnedChrome]) -> String {
    let listing = live
        .iter()
        .map(|s| format!("pid {} project {} session '{}' idle {}s{}", s.pid, s.project.display(), s.session_id, s.idle.as_secs(), if s.keep_alive { " keep_alive" } else { "" }))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "chrome_max_concurrent={cap} gm-launched Chrome processes are already live across every project this daemon serves (launches in flight: {}) and none has been idle for {}s, so no new Chrome was started. Live: [{listing}]. Free one with `sessionId=<id>` plus `session close <id>`, wait for chrome_idle_ttl_seconds to reap an idle one, or raise chrome_max_concurrent in .gm/browser-config.json",
        PENDING_CHROME_LAUNCHES.load(Ordering::Relaxed),
        LRU_EVICTION_IDLE_FLOOR.as_secs()
    )
}

fn reserve_chrome_launch_slot(cfg: &BrowserConfig, engine: crate::browser_engine::Engine, launching_key: &str) -> Result<Option<ChromeLaunchReservation>, String> {
    if engine != crate::browser_engine::Engine::Chrome {
        return Ok(None);
    }
    let cap = effective_chrome_cap(cfg);
    let _admission = CHROME_ADMISSION.lock().unwrap_or_else(|e| e.into_inner());
    let occupied = |live: &[OwnedChrome]| live.len() + PENDING_CHROME_LAUNCHES.load(Ordering::Relaxed);
    let mut live = owned_chrome_sessions();
    if occupied(&live) >= cap {
        reap_idle_sessions_of_every_tracked_project();
        live = owned_chrome_sessions();
    }
    while occupied(&live) >= cap {
        let victim = live
            .iter()
            .filter(|s| s.key != launching_key && !s.keep_alive && s.idle >= LRU_EVICTION_IDLE_FLOOR && !dispatch_in_flight(&s.key))
            .max_by_key(|s| s.idle)
            .map(|s| (s.key.clone(), s.pid, s.idle));
        let Some((victim_key, victim_pid, victim_idle)) = victim else {
            return Err(chrome_cap_exceeded_message(cap, &live));
        };
        let evicted = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&victim_key);
        if let Some(session) = evicted {
            eprintln!(
                "[agentplug browser] evicting least-recently-used idle session '{}' (pid {victim_pid}, project {}, idle {}s) to stay within chrome_max_concurrent={cap}",
                session.session_id,
                session.cwd.display(),
                victim_idle.as_secs()
            );
            kill_session(session);
        }
        live = owned_chrome_sessions();
    }
    PENDING_CHROME_LAUNCHES.fetch_add(1, Ordering::Relaxed);
    Ok(Some(ChromeLaunchReservation))
}

fn evict_session_lifecycle_locks_with_no_active_holder() {
    let mut locks = SESSION_LIFECYCLE_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, arc| Arc::strong_count(arc) > 1);
}

fn session_new(cwd: &Path, session_id: &str, owner_gm_session: Option<&str>, cfg: &BrowserConfig, engine: crate::browser_engine::Engine) -> Value {
    let key = session_key(cwd, session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let _lifecycle_guard = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.remove(&key) {
            kill_session(existing);
        }
    }
    let _launch_slot = match reserve_chrome_launch_slot(cfg, engine, &key) {
        Ok(slot) => slot,
        Err(e) => return json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}),
    };
    match crate::browser_engine::acquire(engine, cwd, session_id, cfg) {
        Ok(acquired) => {
            let pid = acquired.child.as_ref().map(|c| c.id()).unwrap_or(0);
            let port = acquired.port;
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                key,
                BrowserSession {
                    cwd: cwd.to_path_buf(),
                    session_id: session_id.to_string(),
                    owner_gm_session: owner_gm_session.map(str::to_string),
                child: acquired.child,
                pid,
                port,
                cdp_endpoint: acquired.cdp_endpoint,
                    last_used: Instant::now(),
                    target_id: None,
                    owns_process: acquired.owns_process,
                    engine,
                    idle_reap: crate::idle_reap::recorded(&browser_chrome_profile_dir(cwd, session_id)),
                },
            );
            drop(map);
            record_owner_gm_session(cwd, session_id, owner_gm_session);
            json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": session_id, "owner_gm_session": owner_gm_session, "port": port, "idle_reap": IdleReap::report(crate::idle_reap::recorded(&browser_chrome_profile_dir(cwd, session_id)))})
        }
        Err(e) => json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}),
    }
}

fn session_list(cwd: &Path, caller_gm_session: Option<&str>, caller_implicit_session: &str) -> Value {
    let cfg = BrowserConfig::load(cwd);
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys_for_cwd: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.cwd == cwd)
        .map(|(k, _)| k.clone())
        .collect();
    let mut out = Vec::new();
    for k in keys_for_cwd {
        let alive = map.get_mut(&k).map(session_is_alive).unwrap_or(false);
        if !alive {
            map.remove(&k);
            continue;
        }
        if let Some(s) = map.get(&k) {
            let idle_timeout_ms = effective_idle_timeout(s, &cfg).map(|t| t.as_millis() as u64);
            let owner_last_seen_ms = s
                .owner_gm_session
                .as_deref()
                .and_then(crate::dispatch_origin::session_activity_elapsed)
                .map(|d| d.as_millis() as u64);
            out.push(json!({
                "session_id": s.session_id,
                "owner_gm_session": s.owner_gm_session,
                "owned_by_caller": caller_gm_session.is_some() && s.owner_gm_session.as_deref() == caller_gm_session,
                "is_caller_implicit_session": s.session_id == caller_implicit_session,
                "port": s.port,
                "pid": s.pid,
                "project": s.cwd.display().to_string(),
                "owns_process": s.owns_process,
                "alive": true,
                "idle_ms": s.last_used.elapsed().as_millis() as u64,
                "idle_seconds": s.last_used.elapsed().as_secs(),
                "idle_reap": IdleReap::report(s.idle_reap),
                "idle_timeout_ms": idle_timeout_ms,
                "owner_last_dispatch_ms": owner_last_seen_ms,
                "target_id": s.target_id,
                "engine": format!("{:?}", s.engine),
            }));
        }
    }
    drop(map);
    let all_chrome = owned_chrome_sessions();
    let mut pids_to_measure: Vec<u32> = all_chrome.iter().map(|s| s.pid).collect();
    pids_to_measure.extend(out.iter().filter(|v| v["owns_process"] == json!(true)).filter_map(|v| v["pid"].as_u64()).map(|p| p as u32));
    pids_to_measure.sort_unstable();
    pids_to_measure.dedup();
    let working_sets = crate::process_tree::tree_working_set_bytes(&pids_to_measure);
    let working_set_mb = |pid: u32| working_sets.get(&pid).map(|bytes| bytes / (1024 * 1024));
    for entry in &mut out {
        let pid = entry["pid"].as_u64().map(|p| p as u32);
        entry["working_set_mb"] = json!(pid.and_then(working_set_mb));
    }
    let chrome_sessions_all_projects: Vec<Value> = all_chrome
        .iter()
        .map(|s| json!({
            "session_id": s.session_id,
            "project": s.project.display().to_string(),
            "pid": s.pid,
            "idle_seconds": s.idle.as_secs(),
            "keep_alive": s.keep_alive,
            "working_set_mb": working_set_mb(s.pid),
        }))
        .collect();
    json!({
        "ok": true, "stdout": "", "exit_code": 0, "stderr": "",
        "caller_gm_session": caller_gm_session,
        "caller_implicit_session": caller_implicit_session,
        "chrome_process_count": all_chrome.len(),
        "chrome_max_concurrent": effective_chrome_cap(&cfg),
        "chrome_idle_ttl_seconds": cfg.chrome_idle_ttl().as_secs(),
        "chrome_sessions_all_projects": chrome_sessions_all_projects,
        "sessions": out,
    })
}

fn session_close(cwd: &Path, target_session_id: &str, require_found: bool) -> Value {
    let key = session_key(cwd, target_session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let _lifecycle_guard_blocks_concurrent_launch_for_this_key = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
    let remove_tracked = || sessions_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
    let removed = remove_tracked().or_else(|| {
        try_adopt_orphaned_session(cwd, Some(target_session_id), &browser_chrome_profile_dir(cwd, target_session_id))
            .filter(|adopted_id| adopted_id == target_session_id)
            .and_then(|_| remove_tracked())
    });
    crate::gpu::record_uncapped(&browser_chrome_profile_dir(cwd, target_session_id), false);
    crate::idle_reap::record(&browser_chrome_profile_dir(cwd, target_session_id), None);
    match removed {
        Some(session) => {
            kill_session(session);
            json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": target_session_id, "closed": true})
        }
        None if require_found => json!({
            "ok": false, "stdout": "", "exit_code": 1,
            "stderr": format!("no live session found for id '{target_session_id}'"),
            "session_id": target_session_id, "closed": false
        }),
        None => json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": target_session_id, "closed": false}),
    }
}

fn session_ids_owned_by(cwd: &Path, owner_gm_session: &str) -> Vec<String> {
    let mut ids: Vec<String> = sessions_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|s| s.cwd == cwd && s.owner_gm_session.as_deref() == Some(owner_gm_session))
        .map(|s| s.session_id.clone())
        .collect();
    if let Ok(entries) = std::fs::read_dir(browser_profiles_root_for_orphan_scan(cwd)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(suffix) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("browser-chrome-profile-")) else { continue };
            let recorded_owner = std::fs::read_to_string(owner_gm_session_sidecar_path(&path)).ok();
            if recorded_owner.as_deref().map(str::trim) != Some(owner_gm_session) {
                continue;
            }
            let recorded_id = std::fs::read_to_string(session_id_sidecar_path(&path)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            ids.push(recorded_id.unwrap_or_else(|| suffix.to_string()));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

fn session_close_all_owned_by(cwd: &Path, owner_gm_session: &str) -> Value {
    let closed: Vec<String> = session_ids_owned_by(cwd, owner_gm_session)
        .into_iter()
        .filter(|id| session_close(cwd, id, false)["closed"] == json!(true))
        .collect();
    json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "owner_gm_session": owner_gm_session, "closed_count": closed.len(), "closed": closed})
}

enum SessionCommand<'a> {
    CloseAll,
    New,
    List,
    Close(&'a str),
    Reset(&'a str),
    Unknown(&'a str),
    None,
}

fn unknown_session_subcommand<'a>(first_line: &'a str, remainder: &str) -> Option<&'a str> {
    let rest = first_line.strip_prefix("session")?;
    if rest.is_empty() {
        return remainder.trim().is_empty().then_some("");
    }
    let rest = rest.strip_prefix([' ', '\t'])?.trim();
    let word = rest.split_whitespace().next().unwrap_or("");
    let is_command_word = word.starts_with(|c: char| c.is_ascii_alphabetic()) && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    (is_command_word && !matches!(word, "in" | "instanceof")).then_some(rest)
}

fn parse_session_command(body: &str) -> (SessionCommand<'_>, &str) {
    let trimmed = body.trim_start();
    let (first_line, remainder) = match trimmed.find('\n') {
        Some(nl) => (&trimmed[..nl], &trimmed[nl + 1..]),
        None => (trimmed, ""),
    };
    let first_line = first_line.trim_end();
    if first_line == "session new" {
        return (SessionCommand::New, remainder);
    }
    if first_line == "session list" {
        return (SessionCommand::List, remainder);
    }
    if matches!(first_line, "session close-all" | "session close all" | "close-all") {
        return (SessionCommand::CloseAll, remainder);
    }
    if first_line == "session close" || first_line == "session kill" {
        return (SessionCommand::Close(""), remainder);
    }
    if let Some(id) = first_line.strip_prefix("session close ").or_else(|| first_line.strip_prefix("session kill ")) {
        return (SessionCommand::Close(id.trim()), remainder);
    }
    if let Some(id) = first_line.strip_prefix("session reset ") {
        return (SessionCommand::Reset(id.trim()), remainder);
    }
    if first_line == "session reset" {
        return (SessionCommand::Reset(""), remainder);
    }
    if let Some(sub) = unknown_session_subcommand(first_line, remainder) {
        return (SessionCommand::Unknown(sub), remainder);
    }
    (SessionCommand::None, body)
}

fn refuse_trailing_body_after_terminal_session_command(command: &str, remainder: &str) -> Value {
    let dropped_lines = remainder.lines().filter(|l| !l.trim().is_empty()).count();
    json!({"ok": false, "stdout": "", "exit_code": 1,
        "stderr": format!(
            "'{command}' is a terminal session command and the {dropped_lines} non-empty line(s) after it were not evaluated -- send them as their own dispatch, or stack them under 'session new'/'session reset <id>' which do continue into the script"
        )})
}

fn chrome_launch_log_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome-launch.log")
}

#[cfg(unix)]
fn running_as_unix_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_unix_root() -> bool {
    false
}

fn chrome_stderr_log_indicates_suid_sandbox_init_denial(log_path: &Path) -> bool {
    std::fs::read_to_string(log_path)
        .map(|s| {
            s.contains("Failed to move to new namespace")
                || s.contains("Sandbox cannot access executable")
                || s.contains("SUID sandbox helper binary was found, but is not configured correctly")
                || s.contains("running as root without --no-sandbox is not supported")
                || s.contains("No usable sandbox!")
                || s.contains("--no-sandbox")
        })
        .unwrap_or(false)
}

fn is_valid_chrome_extra_arg(arg: &str) -> bool {
    arg.starts_with("--") && !arg.contains(['\0', '\n', '\r'])
}

fn partition_chrome_extra_args(cfg: &BrowserRuntimeConfig) -> (Vec<String>, Vec<String>) {
    let mut accepted = Vec::new();
    let mut dropped = Vec::new();
    for entry in cfg.chrome_extra_args() {
        match entry.as_str().filter(|s| is_valid_chrome_extra_arg(s)) {
            Some(arg) => accepted.push(arg.to_string()),
            None => dropped.push(entry.to_string()),
        }
    }
    (accepted, dropped)
}

fn chrome_launch_args(profile_dir: &Path, port: u16, headless: bool, no_sandbox: bool, cfg: &BrowserRuntimeConfig) -> Result<Vec<String>, String> {
    let mut args = vec![
        format!("--user-data-dir={}", profile_dir.display()),
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-gpu-process-crash-limit".to_string(),
    ];
    if headless {
        if cfg.headless_disable_gpu() {
            args.push("--disable-gpu".to_string());
        }
        args.push("--headless=new".to_string());
        args.push("--ignore-gpu-blocklist".to_string());
        args.push("--enable-unsafe-webgpu".to_string());
        if cfg!(windows) {
            args.push("--use-angle=d3d11".to_string());
        }
    }
    if cfg.enable_webgpu() && !headless {
        args.push("--enable-unsafe-webgpu".to_string());
    }
    args.extend(crate::gpu::launch_args(profile_dir, cfg.configured_gpu(), cfg.uncapped() || crate::gpu::recorded_uncapped(profile_dir))?);
    if no_sandbox {
        args.extend(["--no-sandbox", "--disable-setuid-sandbox", "--disable-dev-shm-usage"].map(String::from));
    }
    if let Some(ext) = cfg.load_extension() {
        args.push(format!("--load-extension={ext}"));
        args.push(format!("--disable-extensions-except={ext}"));
    }
    args.extend(partition_chrome_extra_args(cfg).0);
    let mut seen = std::collections::HashSet::new();
    args.retain(|a| seen.insert(a.clone()));
    Ok(args)
}

fn spawn_chrome_once(
    chrome: &Path,
    profile_dir: &Path,
    port: u16,
    headless: bool,
    no_sandbox: bool,
    cfg: &BrowserRuntimeConfig,
) -> Result<Child, String> {
    let mut cmd = Command::new(chrome);
    cmd.args(chrome_launch_args(profile_dir, port, headless, no_sandbox, cfg)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        crate::windowless::apply_windowless(&mut cmd);
    }
    let log_path = chrome_launch_log_path(profile_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open chrome launch log {}: {e}", log_path.display()))?;
    for rejected in partition_chrome_extra_args(cfg).1 {
        let _ = writeln!(&log_file, "[chrome_extra_args] dropped invalid entry {rejected} (must be a string starting with -- and free of NUL/newline)");
    }
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone chrome launch log handle: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| format!("chrome launch failed: {e}"))
}

pub(crate) fn launch_chrome_pub(cwd: &Path, session_id: &str, browser_cfg: &BrowserRuntimeConfig) -> Result<(Child, u16), String> {
    launch_chrome(cwd, session_id, browser_cfg)
}

pub(crate) fn load_extension_after_launch(cwd: &Path, session_id: &str, port: u16, browser_cfg: &BrowserRuntimeConfig) {
    let Some(ext_path) = browser_cfg.load_extension() else { return };
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let log_path = chrome_launch_log_path(&profile_dir);
    let log_line = |msg: &str| {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(f, "[load_extension] {msg}");
        }
    };
    let Some(node) = which("node") else {
        log_line("node not found on PATH; skipping extension load");
        return;
    };
    let script_path = std::env::temp_dir().join(format!("agentplug-load-extension-{}.mjs", std::process::id()));
    if std::fs::write(&script_path, EXTENSION_LOAD_JS).is_err() {
        log_line("failed to write extension_load.js helper to temp dir");
        return;
    }
    let mut ext_cmd = Command::new(&node);
    ext_cmd.arg(&script_path).arg(port.to_string()).arg(ext_path);
    crate::windowless::apply_windowless(&mut ext_cmd);
    let output = ext_cmd.output();
    let _ = std::fs::remove_file(&script_path);
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout.lines().last().unwrap_or("").trim();
            match serde_json::from_str::<Value>(line) {
                Ok(v) => {
                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                        let _ = std::fs::write(profile_dir.join("chrome.extension-id"), id);
                        log_line(&format!("loaded '{ext_path}' as extension id {id}"));
                    } else if let Some(err) = v.get("error") {
                        log_line(&format!("Extensions.loadUnpacked failed for '{ext_path}': {err}"));
                    } else {
                        log_line(&format!("Extensions.loadUnpacked: unrecognized response: {line}"));
                    }
                }
                Err(_) => log_line(&format!("Extensions.loadUnpacked: non-JSON output: {line} (stderr: {})", String::from_utf8_lossy(&out.stderr))),
            }
        }
        Err(e) => log_line(&format!("failed to spawn node to load extension: {e}")),
    }
}

fn launch_chrome(cwd: &Path, session_id: &str, browser_cfg: &BrowserConfig) -> Result<(Child, u16), String> {
    let chrome = find_chrome().ok_or_else(|| "no Chrome found; install Google Chrome or Chromium".to_string())?;
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let _ = std::fs::create_dir_all(&profile_dir);
    let log_path = chrome_launch_log_path(&profile_dir);
    let _ = std::fs::remove_file(&log_path);

    let no_sandbox_env = std::env::var("GM_BROWSER_NO_SANDBOX").ok();
    let running_as_root = running_as_unix_root();
    let mut no_sandbox = matches!(no_sandbox_env.as_deref(), Some("1"))
        || cfg!(windows)
        || (running_as_root && no_sandbox_env.as_deref() != Some("0"));
    let headless = browser_cfg.headless();
    let port = free_port();
    let mut chrome_child = spawn_chrome_once(&chrome, &profile_dir, port, headless, no_sandbox, browser_cfg)?;

    write_session_sidecars(&profile_dir, chrome_child.id(), port, session_id);

    if !cdp_ready(port, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
        kill_pid(chrome_child.id());
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();

        if !no_sandbox && no_sandbox_env.as_deref() != Some("0") && chrome_stderr_log_indicates_suid_sandbox_init_denial(&log_path) {
            no_sandbox = true;
            let port2 = free_port();
            let mut retry_child = spawn_chrome_once(&chrome, &profile_dir, port2, headless, no_sandbox, browser_cfg)?;
            write_session_sidecars(&profile_dir, retry_child.id(), port2, session_id);
            if cdp_ready(port2, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
                return Ok((retry_child, port2));
            }
            kill_pid(retry_child.id());
            let _ = retry_child.kill();
            let _ = retry_child.wait();
        }

        let _ = std::fs::remove_file(pid_sidecar_path(&profile_dir));
        let _ = std::fs::remove_file(port_sidecar_path(&profile_dir));
        let _ = std::fs::remove_file(session_id_sidecar_path(&profile_dir));
        let log_tail = std::fs::read_to_string(&log_path)
            .ok()
            .map(|s| s.lines().rev().take(5).collect::<Vec<_>>().join(" | "))
            .filter(|s| !s.is_empty());
        return Err(match log_tail {
            Some(tail) => format!(
                "chrome CDP endpoint did not become ready within {}ms (recent chrome output: {tail})",
                browser_cfg.chrome_ready_deadline().as_millis()
            ),
            None => format!(
                "chrome CDP endpoint did not become ready within {}ms (chrome produced no output at all -- it may have failed to spawn)",
                browser_cfg.chrome_ready_deadline().as_millis()
            ),
        });
    }
    Ok((chrome_child, port))
}

pub fn run(body: &str, opts: &str, cwd_raw: &Path, session_id: &str) -> Value {
    let Some(node) = which("node") else {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "node not found on PATH; required to drive Chrome over CDP"});
    };

    let cwd_owned = canonical_project_root(cwd_raw);
    let cwd: &Path = &cwd_owned;

    let t0 = Instant::now();
    ensure_idle_reaper_running();
    let browser_cfg = BrowserConfig::load(cwd);
    reap_idle_sessions(cwd, &browser_cfg);
    reap_os_orphans(cwd);

    let inner_body = body.to_string();
    let opts_v: Value = serde_json::from_str(opts).unwrap_or_else(|_| json!({}));
    let timeout_ms = opts_v.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(120_000);
    let requested_engine = crate::browser_engine::requested_engine_from_envelope(&opts_v);
    let engine = crate::browser_engine::select_engine(cwd, requested_engine.as_deref());
    let requested_cdp_endpoint = crate::browser_engine::chrome_cdp_endpoint_override(cwd);

    let (explicit_session_id, after_session_prefix) = strip_session_id_prefix(&inner_body);
    let inner_body = after_session_prefix;
    let origin = crate::dispatch_origin::current_dispatch_origin();
    let owner_gm_session = origin.gm_session.clone();
    let caller_implicit_session = origin.implicit_page_session(session_id);
    let resolved_session_id = origin.page_session(explicit_session_id, session_id);
    let session_id = resolved_session_id.as_str();

    let (launch_options, launch_option_error, launch_normalized_body) = crate::gpu::split_launch_options(inner_body);
    if let Some(e) = launch_option_error {
        return json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e});
    }
    let inner_body: &str = &launch_normalized_body;
    let gpu_choice = launch_options.gpu;
    let gpu_profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let (session_command, after_session_command) = parse_session_command(inner_body);
    if let SessionCommand::Unknown(sub) = session_command {
        let problem = if sub.is_empty() { "'session' needs a subcommand".to_string() } else { format!("unknown session subcommand '{sub}'") };
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": format!("{problem} -- supported: session new [gpu=<vendor>] [uncapped] | session list | session close [<id>] | session close-all | session reset <id>; no browser was launched")});
    }
    let want_uncapped = launch_options.uncapped || (browser_cfg.uncapped() && matches!(session_command, SessionCommand::None));
    if gpu_choice.is_some() || want_uncapped {
        let key = session_key(cwd, session_id);
        let lifecycle_lock = session_lifecycle_lock_for_key(&key);
        let _launch_mode_change_waits_for_in_flight_eval = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
        let choice_changed = gpu_choice.is_some_and(|c| crate::gpu::recorded_choice(&gpu_profile_dir) != Some(c));
        let uncapped_changed = want_uncapped && !crate::gpu::recorded_uncapped(&gpu_profile_dir);
        if let Some(choice) = gpu_choice {
            crate::gpu::record_choice(&gpu_profile_dir, Some(choice));
        }
        if want_uncapped {
            crate::gpu::record_uncapped(&gpu_profile_dir, true);
        }
        if choice_changed || uncapped_changed {
            let stale = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            if let Some(session) = stale {
                kill_session(session);
            }
        }
    }

    match (&session_command, launch_options.idle_reap) {
        (SessionCommand::New, requested) => crate::idle_reap::record(&gpu_profile_dir, requested),
        (_, Some(requested)) => {
            crate::idle_reap::record(&gpu_profile_dir, Some(requested));
            if let Some(live) = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&session_key(cwd, session_id)) {
                live.idle_reap = Some(requested);
            }
        }
        (_, None) => {}
    }

    let uncapped = browser_cfg.uncapped()
        || match session_command {
            SessionCommand::New => launch_options.uncapped,
            SessionCommand::Reset(id) if id == session_id => false,
            _ => crate::gpu::recorded_uncapped(&gpu_profile_dir),
        };
    let trailing_body_present = !after_session_command.trim().is_empty();
    let mut session_created_by_this_dispatch = false;
    let inner_body: &str = match session_command {
        SessionCommand::New => {
            if gpu_choice.is_none() {
                crate::gpu::record_choice(&gpu_profile_dir, None);
            }
            crate::gpu::record_uncapped(&gpu_profile_dir, launch_options.uncapped || browser_cfg.uncapped());
            let created = session_new(cwd, session_id, owner_gm_session.as_deref(), &browser_cfg, engine);
            if !trailing_body_present || created.get("ok") != Some(&Value::Bool(true)) {
                let mut created = created;
                let created_port = created.get("port").and_then(|p| p.as_u64()).map(|p| p as u16);
                if let (Some(created_port), true) = (created_port, created.get("ok") == Some(&Value::Bool(true)) && engine == crate::browser_engine::Engine::Chrome) {
                    let endpoint = format!("http://127.0.0.1:{created_port}");
                    created["gpu"] = crate::gpu::report(&node, created_port, &endpoint, &gpu_profile_dir, 30_000, uncapped);
                }
                return created;
            }
            session_created_by_this_dispatch = true;
            after_session_command
        }
        SessionCommand::List => {
            if trailing_body_present {
                return refuse_trailing_body_after_terminal_session_command("session list", after_session_command);
            }
            return session_list(cwd, owner_gm_session.as_deref(), &caller_implicit_session);
        }
        SessionCommand::Close(id) if !id.is_empty() => {
            if trailing_body_present {
                return refuse_trailing_body_after_terminal_session_command("session close <id>", after_session_command);
            }
            return session_close(cwd, id, true);
        }
        SessionCommand::Reset(id) if !id.is_empty() => {
            let closed = session_close(cwd, id, false);
            if !trailing_body_present {
                return closed;
            }
            after_session_command
        }
        SessionCommand::Close(_) => {
            if trailing_body_present {
                return refuse_trailing_body_after_terminal_session_command("session close", after_session_command);
            }
            return session_close(cwd, &caller_implicit_session, false);
        }
        SessionCommand::CloseAll => {
            if trailing_body_present {
                return refuse_trailing_body_after_terminal_session_command("session close-all", after_session_command);
            }
            return match owner_gm_session.as_deref() {
                Some(owner) => session_close_all_owned_by(cwd, owner),
                None => session_close(cwd, &caller_implicit_session, false),
            };
        }
        SessionCommand::Reset(_) => {
            return json!({"ok": false, "stdout": "", "exit_code": 1,
                "stderr": format!("session reset requires an explicit id, e.g. 'session reset {caller_implicit_session}' for this gm session's own page")});
        }
        SessionCommand::None | SessionCommand::Unknown(_) => inner_body,
    };

    let mut timeout_override: Option<u64> = None;
    let mut quiet_override: Option<bool> = None;
    let mut mode = BrowserMode::Default;
    let mut mode_name = String::new();
    let mut viewport = None;
    let mut start_url: Option<String> = None;
    let mut url_default_script = String::new();
    let mut rest: &str = inner_body;
    if crate::gpu::is_gpu_query(inner_body) {
        mode = BrowserMode::Gpu;
        rest = "";
    }
    loop {
        let (t, after_timeout) = strip_timeout_prefix(rest);
        if let Some(ms) = t {
            timeout_override = Some(ms);
            rest = after_timeout;
            continue;
        }
        let (q, after_quiet) = strip_debug_visibility_prefix(rest);
        if q.is_some() {
            quiet_override = q;
            rest = after_quiet;
            continue;
        }
        let (m, name, after_mode) = strip_mode_prefix(rest);
        if m != BrowserMode::Default {
            mode = m;
            mode_name = name;
            rest = after_mode;
            continue;
        }
        let (v, after_viewport) = strip_viewport_width_height_scale_mobile_prefix(rest);
        if v.is_some() {
            viewport = v;
            rest = after_viewport;
            continue;
        }
        if start_url.is_none() {
            let (u, default_script, after_url) = strip_url_prefix(rest);
            if u.is_some() {
                start_url = u;
                url_default_script = default_script;
                rest = after_url;
                continue;
            }
        }
        break;
    }
    let dom_selector = mode_name.clone();
    let quiet_debug = quiet_override.unwrap_or(mode != BrowserMode::Capture);
    let timeout_ms = timeout_override.unwrap_or(timeout_ms);
    let script = if mode == BrowserMode::Gpu {
        "void 0".to_string()
    } else if rest.trim().is_empty() {
        if url_default_script.trim().is_empty() && start_url.is_some() {
            "void 0".to_string()
        } else {
            url_default_script
        }
    } else {
        rest.to_string()
    };

    if mode != BrowserMode::Dom && script.trim().is_empty() {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "browser dispatch resolved to an empty script body after prefix parsing -- nothing would be evaluated, refusing rather than silently launching/reusing a session and returning a false success",
            "start_url": start_url});
    }

    let key = session_key(cwd, session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let (_lifecycle_guard_serializes_temp_files_reuse_check_launch_and_insert, queued_behind_same_page_ms) = match lifecycle_lock.try_lock() {
        Ok(guard) => (guard, None),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => (poisoned.into_inner(), None),
        Err(std::sync::TryLockError::WouldBlock) => {
            let queued_at = Instant::now();
            let guard = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
            let waited_ms = queued_at.elapsed().as_millis() as u64;
            eprintln!(
                "[agentplug browser] queued {waited_ms}ms behind another dispatch on the same page (session '{session_id}'); use `sessionId=<other>` for an independent page"
            );
            (guard, Some(waited_ms))
        }
    };

    let tmp = std::env::temp_dir();
    let stamp = format!("{}-{}", std::process::id(), sanitize(session_id));
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    let artifact_path = match mode {
        BrowserMode::Default | BrowserMode::Dom | BrowserMode::Gpu => None,
        BrowserMode::Screenshot => {
            let dir = cwd.join(".gm").join("witness");
            let _ = std::fs::create_dir_all(&dir);
            let stem = if mode_name.trim().is_empty() {
                format!("{}-{}", mode_label(mode), unix_ms())
            } else {
                sanitize(mode_name.trim())
            };
            Some(dir.join(format!("{stem}.png")))
        }
        _ => {
            let dir = browser_profiles_dir(cwd);
            let _ = std::fs::create_dir_all(&dir);
            let ext = match mode { BrowserMode::Trace => "trace.json", _ => "profile.json" };
            Some(dir.join(format!("{}-{}.{}", mode_label(mode), unix_ms(), ext)))
        }
    };
    if let Ok(mut f) = std::fs::File::create(&helper_path) {
        let _ = f.write_all(CDP_EVAL_JS.as_bytes());
    }
    let gpu_probe_path = (mode == BrowserMode::Gpu).then(|| tmp.join(format!("agentplug-gpu-probe-{stamp}.js")));
    if let Some(path) = &gpu_probe_path {
        let _ = std::fs::write(path, crate::gpu::GPU_PROBE_JS);
    }
    if let Ok(mut f) = std::fs::File::create(&script_path) {
        let _ = f.write_all(script.as_bytes());
    }

    let engine_mismatch = {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&key).map(|s| (s.engine, s.cdp_endpoint.clone())).filter(|(prior_engine, prior_endpoint)| {
            *prior_engine != engine || requested_cdp_endpoint.as_deref().is_some_and(|endpoint| endpoint != prior_endpoint)
        })
    };
    if let Some((prior_engine, _)) = engine_mismatch {
        let stale = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(session) = stale {
            eprintln!(
                "[agentplug browser] session {key} was created under {prior_engine:?} but this dispatch asked for {engine:?} -- evicting and relaunching rather than answering from the wrong engine"
            );
            kill_session(session);
        }
    }
    let candidate_port = {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        let reuse_port = map.get_mut(&key).and_then(|s| {
            if session_is_alive(s) {
                Some(s.port)
            } else {
                None
            }
        });
        if reuse_port.is_none() {
            map.remove(&key);
        }
        reuse_port
    };
    let mut known_target_id: Option<String> = None;
    let mut launched_fresh_chrome = false;
    let session_had_prior_page = target_id_sidecar_path(&browser_chrome_profile_dir(cwd, session_id)).exists();
    let port = match candidate_port.filter(|_| {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&key).is_some_and(|session| session_cdp_endpoint_responds(&session.cdp_endpoint))
    }) {
        Some(p) => {
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = map.get_mut(&key) {
                s.last_used = Instant::now();
                known_target_id = s.target_id.clone();
            }
            Some(p)
        }
        None => {
            if candidate_port.is_some() {
                let stale = sessions_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                if let Some(session) = stale {
                    eprintln!(
                        "[agentplug browser] evicting tracked session {key} -- process alive but CDP endpoint unresponsive, falling back to a fresh spawn"
                    );
                    kill_session(session);
                }
            }
            None
        }
    };
    let port = match port {
        Some(p) => p,
        None => {
            let adopted_port = if engine == crate::browser_engine::Engine::Steel {
                None
            } else {
                try_adopt_orphaned_session(cwd, Some(session_id), &browser_chrome_profile_dir(cwd, session_id))
                    .filter(|adopted_id| adopted_id == session_id)
                    .and_then(|adopted_id| {
                        let adopted_key = session_key(cwd, &adopted_id);
                        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
                        map.get_mut(&adopted_key).map(|s| {
                            s.last_used = Instant::now();
                            known_target_id = s.target_id.clone();
                            s.port
                        })
                    })
            };
            match adopted_port {
                Some(p) => p,
                None => {
                    let _launch_slot = match reserve_chrome_launch_slot(&browser_cfg, engine, &key) {
                        Ok(slot) => slot,
                        Err(e) => {
                            cleanup(&[&helper_path, &script_path, &result_path]);
                            return annotate_queue_wait(json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}), queued_behind_same_page_ms, session_id);
                        }
                    };
                    let acquired = match crate::browser_engine::acquire(engine, cwd, session_id, &browser_cfg) {
                        Ok(v) => v,
                        Err(e) => {
                            cleanup(&[&helper_path, &script_path, &result_path]);
                            return annotate_queue_wait(json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}), queued_behind_same_page_ms, session_id);
                        }
                    };
                    launched_fresh_chrome = true;
                    let pid = acquired.child.as_ref().map(|c| c.id()).unwrap_or(0);
                    let new_port = acquired.port;
                    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
                    map.insert(
                        key.clone(),
                        BrowserSession {
                            cwd: cwd.to_path_buf(),
                            session_id: session_id.to_string(),
                            owner_gm_session: owner_gm_session.clone(),
                            child: acquired.child,
                            pid,
                            port: new_port,
                            cdp_endpoint: acquired.cdp_endpoint,
                            last_used: Instant::now(),
                            target_id: None,
                            owns_process: acquired.owns_process,
                            engine,
                            idle_reap: crate::idle_reap::recorded(&browser_chrome_profile_dir(cwd, session_id)),
                        },
                    );
                    drop(map);
                    record_owner_gm_session(cwd, session_id, owner_gm_session.as_deref());
                    launched_fresh_chrome = true;
                    new_port
                }
            }
        }
    };

    let cfg = json!({
        "port": port,
        "cdpEndpoint": sessions_map().lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|session| session.cdp_endpoint.clone()).unwrap_or_else(|| format!("http://127.0.0.1:{port}")),
        "startUrl": start_url,
        "targetId": known_target_id,
        "claimFreshTarget": engine == crate::browser_engine::Engine::Steel,
        "glCapture": mode == BrowserMode::Capture && mode_name == "gl",
        "uncapped": uncapped,
        "gpuProbeFile": gpu_probe_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "wantGpu": crate::gpu::recorded_choice(&gpu_profile_dir).filter(|c| *c != crate::gpu::GpuChoice::Default).map(crate::gpu::GpuChoice::label),
        "scriptFile": script_path.to_string_lossy(),
        "resultFile": result_path.to_string_lossy(),
        "timeoutMs": timeout_ms,
        "mode": mode_label(mode),
        "artifactFile": artifact_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "domSelector": dom_selector,
        "viewport": viewport.map(|(width, height, device_scale_factor, mobile)| json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": device_scale_factor,
            "mobile": mobile,
        })),
    })
    .to_string();

    let mut spawn_cmd = Command::new(&node);
    spawn_cmd.arg(&helper_path)
        .arg(&cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        crate::windowless::apply_windowless(&mut spawn_cmd);
    }
    let spawn = spawn_cmd.spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            cleanup(&[&helper_path, &script_path, &result_path]);
            return annotate_queue_wait(
                json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": format!("node cdp helper spawn failed: {e}")}),
                queued_behind_same_page_ms,
                session_id,
            );
        }
    };

    let timed_out = match child.wait_timeout(Duration::from_millis(timeout_ms + browser_cfg.eval_timeout_grace())) {
        Ok(Some(_)) => false,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        Err(_) => false,
    };

    let cdp_endpoint = sessions_map().lock().unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|session| session.cdp_endpoint.clone())
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    if timed_out && !session_liveness_recheck(port, &cdp_endpoint, &browser_cfg) {
        eprintln!(
            "[agentplug browser] eval timeout AND page unresponsive to a follow-up probe -- session '{}' (port {}) is wedged, killing and evicting so the next dispatch gets a fresh Chrome",
            session_id, port
        );
        let dead = {
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&key)
        };
        if let Some(session) = dead {
            kill_session(session);
        }
    }

    let mut stderr_buf = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_end(&mut err, &mut stderr_buf);
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);

    let result_value: Value = std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);

    if let Some(resolved_target_id) = result_value.get("__targetId").and_then(|v| v.as_str()) {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get_mut(&key) {
            s.target_id = Some(resolved_target_id.to_string());
        }
        write_target_id_sidecar(&browser_chrome_profile_dir(cwd, session_id), resolved_target_id);
    }

    cleanup(&[&helper_path, &script_path, &result_path]);
    if let Some(path) = &gpu_probe_path {
        cleanup(&[path.as_path()]);
    }
    touch_last_used(cwd, session_id, &key);

    let cdp_error =result_value.get("__cdpError").and_then(|v| v.as_str());
    let ok = exit_code == 0 && !timed_out && cdp_error.is_none();
    let default_debug = || json!({"instrumented": false, "hint": "no console/network/pageError capture in this mode; prefix the body with `capture` (or `capture gl` for GL error tracking: draw calls are counted and getError is drained once per animation frame, errors are attributed to the frame's last draw and re-served to the page's own getError) to collect it"});
    let shaped_debug = |raw: Option<&Value>| -> Value {
        let debug = raw.cloned().unwrap_or_else(default_debug);
        if quiet_debug && debug.get("instrumented") != Some(&Value::Bool(false)) { compact_debug(&debug) } else { debug }
    };
    let mut out = json!({
        "ok": ok,
        "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": t0.elapsed().as_millis() as u64,
        "session_id": session_id,
        "gm_session": owner_gm_session,
        "port": port,
    });
    if session_created_by_this_dispatch {
        out["session_created"] = Value::Bool(true);
    }
    if launched_fresh_chrome && session_had_prior_page && start_url.is_none() && !session_created_by_this_dispatch {
        out["session_recycled"] = Value::Bool(true);
        out["session_note"] = json!("this dispatch launched a fresh browser for the session (the previous one was reaped idle, evicted at the concurrent-Chrome cap, or died), so earlier page state is gone; pass url=<target> on every call that depends on a loaded page");
    }
    let mut out = annotate_queue_wait(out, queued_behind_same_page_ms, session_id);
    if cdp_error.is_some() {
        out["result"] = Value::Null;
        out["debug"] = shaped_debug(result_value.get("debug"));
        return out;
    }
    let launched_real_page = matches!(mode, BrowserMode::Dom) || result_value.get("result").is_some() || result_value.get("elements").is_some();
    match mode {
        BrowserMode::Default => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = shaped_debug(result_value.get("debug"));
        }
        BrowserMode::Capture => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = shaped_debug(result_value.get("debug"));
        }
        BrowserMode::Profile => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["profile"] = result_value.get("profile").cloned().unwrap_or(json!({"timeframe": null, "culprits": []}));
            out["debug"] = shaped_debug(result_value.get("debug"));
            if let Some(p) = &artifact_path {
                out["profile_file"] = json!(p.to_string_lossy());
            }
        }
        BrowserMode::Trace => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["trace"] = result_value.get("trace").cloned().unwrap_or(json!({"wall_us": 0, "gpu_us": 0, "viz_us": 0, "cc_us": 0, "by_category": {}}));
            out["debug"] = shaped_debug(result_value.get("debug"));
            if let Some(p) = &artifact_path {
                out["trace_file"] = json!(p.to_string_lossy());
            }
        }
        BrowserMode::Screenshot => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = shaped_debug(result_value.get("debug"));
            let screenshot_error = result_value.get("screenshot_error").cloned().filter(|e| !e.is_null());
            match (&artifact_path, screenshot_error) {
                (Some(p), None) => {
                    out["screenshot_path"] = json!(p.to_string_lossy());
                }
                (_, Some(e)) => {
                    out["screenshot_error"] = e;
                }
                (None, None) => {}
            }
        }
        BrowserMode::Gpu => {
            out["result"] = crate::gpu::with_display_probe(result_value.get("result").cloned().unwrap_or(Value::Null), uncapped);
        }
        BrowserMode::Dom => {
            out["selector"] = json!(dom_selector);
            out["match_count"] = result_value.get("match_count").cloned().unwrap_or(json!(0));
            out["elements"] = result_value.get("elements").cloned().unwrap_or(json!([]));
            out["debug"] = shaped_debug(result_value.get("debug"));
            if let Some(e) = result_value.get("error") {
                if !e.is_null() {
                    out["result"] = json!({ "error": e });
                }
            }
        }
    }
    if let Some(note) = result_value.get("result_note") {
        out["result_note"] = note.clone();
    }
    if requested_engine.as_deref() == Some("lightpanda") && engine == crate::browser_engine::Engine::Chrome {
        out["engine_note"] = json!("lightpanda has no native Windows binary here; the browser verb was served by local Chrome, exactly as the cdp verb would");
    }
    if ok && mode != BrowserMode::Gpu && engine == crate::browser_engine::Engine::Chrome && (launched_fresh_chrome || session_created_by_this_dispatch) {
        out["gpu"] = crate::gpu::report(&node, port, &cdp_endpoint, &gpu_profile_dir, 30_000, uncapped);
    }
    if ok && !launched_real_page {
        out["ok"] = json!(false);
        out["stderr"] = json!(format!(
            "browser dispatch returned success with no evidence a page was ever reached (empty result envelope, mode={}) -- treating as a false success rather than a silent no-op",
            mode_label(mode)
        ));
    }
    out
}

fn annotate_queue_wait(mut out: Value, queued_behind_same_page_ms: Option<u64>, session_id: &str) -> Value {
    if let Some(waited_ms) = queued_behind_same_page_ms {
        out["queued_behind_same_page_dispatch_ms"] = json!(waited_ms);
        out["queue_note"] = json!(format!(
            "queued {waited_ms}ms behind another dispatch on the same page (session '{session_id}'); dispatches of one session run one at a time, use `sessionId=<other>` for an independent page"
        ));
    }
    out
}

fn mode_label(mode: BrowserMode) -> &'static str {
    match mode {
        BrowserMode::Default => "default",
        BrowserMode::Capture => "capture",
        BrowserMode::Profile => "profile",
        BrowserMode::Trace => "trace",
        BrowserMode::Screenshot => "screenshot",
        BrowserMode::Dom => "dom",
        BrowserMode::Gpu => "gpu",
    }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub(crate) fn sanitize_pub(s: &str) -> String { sanitize(s) }

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn cleanup(paths: &[&Path]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}
