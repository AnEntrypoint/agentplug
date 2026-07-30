use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

const CDP_EVAL_JS: &str = include_str!("cdp_eval.js");

#[derive(serde::Deserialize)]
struct BrowserConfig {
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
}

impl BrowserConfig {
    fn load(cwd: &Path) -> Self {
        let path = cwd.join(".gm").join("browser-config.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BrowserConfig>(&s).ok())
            .unwrap_or(BrowserConfig {
                cdp_poll_timeout_ms: None,
                cdp_poll_interval_ms: None,
                chrome_ready_deadline_ms: None,
                eval_timeout_grace_ms: None,
                headless: None,
                session_idle_timeout_ms: None,
            })
    }
    fn cdp_poll_timeout(&self) -> Duration { Duration::from_millis(self.cdp_poll_timeout_ms.unwrap_or(1000)) }
    fn cdp_poll_interval(&self) -> Duration { Duration::from_millis(self.cdp_poll_interval_ms.unwrap_or(250)) }
    fn chrome_ready_deadline(&self) -> Duration { Duration::from_millis(self.chrome_ready_deadline_ms.unwrap_or(30_000)) }
    fn eval_timeout_grace(&self) -> u64 { self.eval_timeout_grace_ms.unwrap_or(6000) }
    fn headless(&self) -> bool { self.headless.unwrap_or(false) }
    fn session_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.session_idle_timeout_ms.unwrap_or(30 * 60 * 1000))
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

fn find_chrome() -> Option<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ]
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

fn cdp_ready(port: u16, deadline: Instant, cfg: &BrowserConfig) -> bool {
    while Instant::now() < deadline {
        let url = format!("http://127.0.0.1:{port}/json/version");
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

fn parse_body(body: &str) -> (Option<String>, String) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("url=") {
        if let Some(nl) = rest.find('\n') {
            return (Some(rest[..nl].trim().to_string()), rest[nl + 1..].to_string());
        }
        return (Some(rest.trim().to_string()), String::new());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        if let Some(nl) = trimmed.find('\n') {
            return (Some(trimmed[..nl].trim().to_string()), trimmed[nl + 1..].to_string());
        }
        return (Some(trimmed.trim().to_string()), "return {url: location.href};".to_string());
    }
    (None, body.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum BrowserMode {
    Default,
    Capture,
    Profile,
    Trace,
    Screenshot,
    Dom,
}

fn strip_mode_prefix(body: &str) -> (BrowserMode, String, &str) {
    let trimmed = body.trim_start();
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
    if let Some(rest) = trimmed.strip_prefix("dom=") {
        let (selector, remainder) = match rest.find('\n') {
            Some(nl) => (rest[..nl].trim().to_string(), &rest[nl + 1..]),
            None => (rest.trim().to_string(), ""),
        };
        return (BrowserMode::Dom, selector, remainder);
    }
    (BrowserMode::Default, String::new(), body)
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
    // An explicit-but-empty `sessionId=` line (caller intends "no override",
    // just wrote the prefix with nothing after it) must still be stripped --
    // returning the original `body` here left the literal "sessionId=\n"
    // text in front of whatever followed, which the downstream session-command/
    // JS parser then choked on as malformed input instead of treating it as
    // "no explicit id". Only the id-with-content branch is genuinely
    // meaningful to keep, so both branches now consume the prefix line.
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
    child: Child,
    port: u16,
    last_used: Instant,
    target_id: Option<String>,
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

fn session_is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

fn session_cdp_endpoint_responds(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/json/version");
    match ureq::get(&url).timeout(std::time::Duration::from_millis(1500)).call() {
        Ok(resp) => resp.into_string().map(|b| b.contains("webSocketDebuggerUrl")).unwrap_or(false),
        Err(_) => false,
    }
}

fn kill_session(mut session: BrowserSession) {
    let _ = session.child.kill();
    let _ = session.child.wait();
    let _ = std::fs::remove_file(pid_sidecar_path(&browser_chrome_profile_dir(&session.cwd, &session.session_id)));
}

fn pid_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.pid")
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        Err(_) => true,
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
        let sidecar_pids: Vec<u32> = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .into_iter()
            .collect();
        let pids_to_check = if sidecar_pids.is_empty() {
            find_pids_of_chrome_processes_using_profile_dir(&path)
        } else {
            sidecar_pids
        };
        let used_sidecar_missing_fallback = !sidecar.exists();
        for pid in pids_to_check {
            if pid_is_alive(pid) {
                eprintln!(
                    "[agentplug browser] reaping OS-orphaned chrome pid={} (profile {}, no owning session in this process -- crash/hard-exit orphan, sidecar-missing fallback used: {})",
                    pid,
                    path.display(),
                    used_sidecar_missing_fallback
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

fn session_liveness_recheck(port: u16, browser_cfg: &BrowserConfig) -> bool {
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
    let recheck_timeout_ms: u64 = 5000;
    let cfg = json!({
        "port": port,
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
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        spawn_cmd.creation_flags(CREATE_NO_WINDOW);
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
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

#[cfg(windows)]
fn list_chrome_processes() -> Vec<(u32, String)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | ForEach-Object { \"$($_.ProcessId)|$($_.CommandLine)\" }",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(o) = output else { return Vec::new() };
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
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

fn find_pids_of_chrome_processes_using_profile_dir(profile_dir: &Path) -> Vec<u32> {
    let needle = profile_dir.to_string_lossy().replace('\\', "/").to_lowercase();
    list_chrome_processes()
        .into_iter()
        .filter_map(|(pid, cmdline)| {
            let cmdline_normalized = cmdline.replace('\\', "/").to_lowercase();
            if cmdline_normalized.contains(&needle) { Some(pid) } else { None }
        })
        .collect()
}

fn cmdline_flag_value(cmdline: &str, flag: &str) -> Option<String> {
    for token in cmdline.split_whitespace() {
        let token = token.trim_matches('"');
        if let Some(rest) = token.strip_prefix(flag) {
            return Some(rest.trim_matches('"').to_string());
        }
    }
    None
}

fn is_headless_root_chrome_process(cmdline: &str) -> bool {
    let has_headless = cmdline.contains("--headless");
    let has_debug_port = cmdline.contains("--remote-debugging-port=");
    let has_user_data_dir = cmdline.contains("--user-data-dir=");
    let is_child_process = cmdline.contains("--type=");
    has_headless && has_debug_port && has_user_data_dir && !is_child_process
}

fn reap_globally_orphaned_headless_chromes() {
    if !this_process_is_the_registered_daemon() {
        eprintln!(
            "[agentplug browser] skipping global headless-orphan reap -- a different process is the fresh registered daemon, this process cannot safely judge liveness of sessions it does not own"
        );
        return;
    }
    let real_user_profile = dirs_local_appdata_chrome_user_data();
    let claimed_profile_dirs: std::collections::HashSet<String> = {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .map(|s| browser_chrome_profile_dir(&s.cwd, &s.session_id).to_string_lossy().replace('\\', "/").to_lowercase())
            .collect()
    };
    for (pid, cmdline) in list_chrome_processes() {
        if !is_headless_root_chrome_process(&cmdline) {
            continue;
        }
        let Some(profile_dir) = cmdline_flag_value(&cmdline, "--user-data-dir=") else { continue };
        let profile_dir_normalized = profile_dir.replace('\\', "/").to_lowercase();
        if let Some(real) = &real_user_profile {
            if profile_dir_normalized == *real {
                continue;
            }
        }
        if claimed_profile_dirs.contains(&profile_dir_normalized) {
            continue;
        }
        if !pid_is_alive(pid) {
            continue;
        }
        eprintln!(
            "[agentplug browser] reaping globally-orphaned headless chrome pid={} (profile {} not tracked by any live session in this process)",
            pid, profile_dir
        );
        kill_pid(pid);
    }
}

#[cfg(windows)]
fn dirs_local_appdata_chrome_user_data() -> Option<String> {
    let local_appdata = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local_appdata)
            .join("Google")
            .join("Chrome")
            .join("User Data")
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase(),
    )
}

#[cfg(not(windows))]
fn dirs_local_appdata_chrome_user_data() -> Option<String> {
    None
}

pub fn close_all_sessions() {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        if let Some(session) = map.remove(&k) {
            eprintln!("[agentplug browser] closing session {} for handoff/shutdown", session.session_id);
            kill_session(session);
        }
    }
}

pub fn reap_idle_sessions_and_os_orphans_across_every_known_project_root(roots: &[std::path::PathBuf]) {
    for root in roots {
        let cfg = BrowserConfig::load(root);
        reap_idle_sessions(root, &cfg);
        reap_os_orphans(root);
    }
    reap_globally_orphaned_headless_chromes();
}

fn reap_idle_sessions(cwd: &Path, cfg: &BrowserConfig) {
    let timeout = cfg.session_idle_timeout();
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let dead_keys: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.cwd == cwd && s.last_used.elapsed() > timeout)
        .map(|(k, _)| k.clone())
        .collect();
    for k in dead_keys {
        if let Some(session) = map.remove(&k) {
            eprintln!(
                "[agentplug browser] reaping idle session {} (idle {}ms > {}ms)",
                session.session_id,
                session.last_used.elapsed().as_millis(),
                timeout.as_millis()
            );
            kill_session(session);
        }
    }
    drop(map);
    evict_session_lifecycle_locks_with_no_active_holder();
}

fn evict_session_lifecycle_locks_with_no_active_holder() {
    let mut locks = SESSION_LIFECYCLE_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, arc| Arc::strong_count(arc) > 1);
}

fn session_new(cwd: &Path, session_id: &str, cfg: &BrowserConfig) -> Value {
    let key = session_key(cwd, session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let _lifecycle_guard = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.remove(&key) {
            kill_session(existing);
        }
    }
    match launch_chrome(cwd, session_id, cfg) {
        Ok((child, port)) => {
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                key,
                BrowserSession {
                    cwd: cwd.to_path_buf(),
                    session_id: session_id.to_string(),
                    child,
                    port,
                    last_used: Instant::now(),
                    target_id: None,
                },
            );
            json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": session_id, "port": port})
        }
        Err(e) => json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}),
    }
}

fn session_list(cwd: &Path) -> Value {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys_for_cwd: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.cwd == cwd)
        .map(|(k, _)| k.clone())
        .collect();
    let mut out = Vec::new();
    for k in keys_for_cwd {
        let alive = map.get_mut(&k).map(|s| session_is_alive(&mut s.child)).unwrap_or(false);
        if !alive {
            map.remove(&k);
            continue;
        }
        if let Some(s) = map.get(&k) {
            out.push(json!({
                "session_id": s.session_id,
                "port": s.port,
                "alive": true,
                "idle_ms": s.last_used.elapsed().as_millis() as u64,
                "target_id": s.target_id,
            }));
        }
    }
    json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "sessions": out})
}

fn session_close(cwd: &Path, target_session_id: &str, require_found: bool) -> Value {
    let key = session_key(cwd, target_session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let _lifecycle_guard_blocks_concurrent_launch_for_this_key = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
    let removed = {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&key)
    };
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

enum SessionCommand<'a> {
    New,
    List,
    Close(&'a str),
    Reset(&'a str),
    None,
}

fn parse_session_command(body: &str) -> SessionCommand<'_> {
    let trimmed = body.trim();
    if trimmed == "session new" || trimmed.starts_with("session new\n") {
        return SessionCommand::New;
    }
    if trimmed == "session list" || trimmed.starts_with("session list\n") {
        return SessionCommand::List;
    }
    if let Some(rest) = trimmed.strip_prefix("session close ") {
        let id = rest.lines().next().unwrap_or("").trim();
        return SessionCommand::Close(id);
    }
    if let Some(rest) = trimmed.strip_prefix("session reset ") {
        let id = rest.lines().next().unwrap_or("").trim();
        return SessionCommand::Reset(id);
    }
    SessionCommand::None
}

fn chrome_launch_log_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome-launch.log")
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

fn spawn_chrome_once(
    chrome: &Path,
    profile_dir: &Path,
    port: u16,
    headless: bool,
    no_sandbox: bool,
) -> Result<Child, String> {
    let mut cmd = Command::new(chrome);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-default-apps")
        .arg("--disable-gpu-process-crash-limit");
    if headless {
        cmd.arg("--disable-gpu").arg("--headless=new");
    }
    if no_sandbox {
        cmd.arg("--no-sandbox").arg("--disable-setuid-sandbox").arg("--disable-dev-shm-usage");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let log_path = chrome_launch_log_path(profile_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open chrome launch log {}: {e}", log_path.display()))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone chrome launch log handle: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| format!("chrome launch failed: {e}"))
}

fn launch_chrome(cwd: &Path, session_id: &str, browser_cfg: &BrowserConfig) -> Result<(Child, u16), String> {
    let chrome = find_chrome().ok_or_else(|| "no Chrome found; install Google Chrome or Chromium".to_string())?;
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let _ = std::fs::create_dir_all(&profile_dir);
    let log_path = chrome_launch_log_path(&profile_dir);
    let _ = std::fs::remove_file(&log_path);

    let no_sandbox_env = std::env::var("GM_BROWSER_NO_SANDBOX").ok();
    let mut no_sandbox = matches!(no_sandbox_env.as_deref(), Some("1")) || cfg!(windows);
    let headless = browser_cfg.headless();
    let port = free_port();
    let mut chrome_child = spawn_chrome_once(&chrome, &profile_dir, port, headless, no_sandbox)?;

    let _ = std::fs::write(pid_sidecar_path(&profile_dir), chrome_child.id().to_string());

    if !cdp_ready(port, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();

        if !no_sandbox && no_sandbox_env.as_deref() != Some("0") && chrome_stderr_log_indicates_suid_sandbox_init_denial(&log_path) {
            no_sandbox = true;
            let port2 = free_port();
            let mut retry_child = spawn_chrome_once(&chrome, &profile_dir, port2, headless, no_sandbox)?;
            let _ = std::fs::write(pid_sidecar_path(&profile_dir), retry_child.id().to_string());
            if cdp_ready(port2, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
                return Ok((retry_child, port2));
            }
            let _ = retry_child.kill();
            let _ = retry_child.wait();
        }

        let _ = std::fs::remove_file(pid_sidecar_path(&profile_dir));
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

pub fn run(body: &str, cwd: &Path, session_id: &str) -> Value {
    let Some(node) = which("node") else {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "node not found on PATH; required to drive Chrome over CDP"});
    };

    let t0 = Instant::now();
    let browser_cfg = BrowserConfig::load(cwd);
    reap_idle_sessions(cwd, &browser_cfg);
    reap_os_orphans(cwd);

    let (inner_body, timeout_ms): (String, u64) = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(obj)) => {
            let b = obj
                .get("body")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let t = obj.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(120_000);
            (b, t)
        }
        _ => (body.to_string(), 120_000),
    };

    let (explicit_session_id, after_session_prefix) = strip_session_id_prefix(&inner_body);
    let session_id_owned;
    let session_id = match &explicit_session_id {
        Some(explicit) => {
            session_id_owned = explicit.clone();
            session_id_owned.as_str()
        }
        None => session_id,
    };
    let inner_body = after_session_prefix;

    // The wasm-side caller (rs-plugkit's `browser` verb handler) resolves a
    // session id from an explicit `sessionId`, the current gm dispatch's own
    // SESSION_ID, or `.gm/exec-spool/.session-current` -- and falls back to
    // an empty string when none of those are set (e.g. a bare eval dispatch
    // with no `session ...` prefix and no prior browser session recorded for
    // this repo). Threading that empty string straight into
    // session_new/session_key used to key the new Chrome session under the
    // literal empty string: session_new and session_list both echoed
    // session_id: "" back, so the caller had no real id to pass to a later
    // `session close <id>`/`session reset <id>` (which require a non-empty
    // explicit id).
    //
    // A prior fix generated a fresh `auto-{pid}-{unix_ms}` id per empty-sid
    // call to make every session addressable. That traded one bug for a
    // worse one: EVERY bare-eval dispatch (the common case -- no explicit
    // `session ...` prefix) minted a brand-new id, so it always missed the
    // session-cache lookup below and always launched a brand-new Chrome
    // process + profile dir, even for back-to-back dispatches against the
    // same repo seconds apart. Observed live: 21 leaked chrome.exe processes
    // from a handful of plain-eval `browser` dispatches in one session.
    //
    // Fix: default to a STABLE, deterministic per-cwd+pid id ("default")
    // instead of a time-varying one. This keeps every property the addressability
    // fix wanted (a real non-empty id, usable with `session close`/`session
    // reset`) while making repeat bare-eval calls resolve to the SAME
    // session_key -> the SAME cached, already-launched Chrome -- process reuse
    // for the overwhelmingly common no-explicit-session case, with the well-worn
    // idle-timeout/reap machinery below still cleaning it up eventually.
    let session_id_owned2;
    let session_id = if session_id.is_empty() {
        session_id_owned2 = "default".to_string();
        session_id_owned2.as_str()
    } else {
        session_id
    };

    match parse_session_command(inner_body) {
        SessionCommand::New => return session_new(cwd, session_id, &browser_cfg),
        SessionCommand::List => return session_list(cwd),
        SessionCommand::Close(id) if !id.is_empty() => return session_close(cwd, id, true),
        SessionCommand::Reset(id) if !id.is_empty() => return session_close(cwd, id, false),
        SessionCommand::Close(_) | SessionCommand::Reset(_) => {
            return json!({"ok": false, "stdout": "", "exit_code": 1,
                "stderr": "session close/reset requires an explicit id, e.g. 'session close default'"});
        }
        SessionCommand::None => {}
    }

    let (timeout_override, after_timeout) = strip_timeout_prefix(inner_body);
    let timeout_ms = timeout_override.unwrap_or(timeout_ms);
    let (mode, dom_selector, after_mode) = strip_mode_prefix(after_timeout);
    let (viewport, after_viewport) = strip_viewport_width_height_scale_mobile_prefix(after_mode);
    let (start_url, script) = parse_body(after_viewport);

    if mode != BrowserMode::Dom && script.trim().is_empty() {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "browser dispatch resolved to an empty script body after prefix parsing -- nothing would be evaluated, refusing rather than silently launching/reusing a session and returning a false success",
            "start_url": start_url});
    }

    let tmp = std::env::temp_dir();
    let stamp = format!("{}-{}", std::process::id(), sanitize(session_id));
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    let artifact_path = match mode {
        BrowserMode::Default | BrowserMode::Dom => None,
        BrowserMode::Screenshot => {
            let dir = cwd.join(".gm").join("witness");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir.join(format!("{}-{}.png", mode_label(mode), unix_ms())))
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
    if let Ok(mut f) = std::fs::File::create(&script_path) {
        let _ = f.write_all(script.as_bytes());
    }

    let key = session_key(cwd, session_id);
    let lifecycle_lock = session_lifecycle_lock_for_key(&key);
    let _lifecycle_guard_serializes_reuse_check_launch_and_insert = lifecycle_lock.lock().unwrap_or_else(|e| e.into_inner());
    let candidate_port = {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        let reuse_port = map.get_mut(&key).and_then(|s| {
            if session_is_alive(&mut s.child) {
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
    let port = match candidate_port.filter(|&p| session_cdp_endpoint_responds(p)) {
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
            let (chrome_child, new_port) = match launch_chrome(cwd, session_id, &browser_cfg) {
                Ok(v) => v,
                Err(e) => {
                    cleanup(&[&helper_path, &script_path, &result_path]);
                    return json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e});
                }
            };
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                key.clone(),
                BrowserSession {
                    cwd: cwd.to_path_buf(),
                    session_id: session_id.to_string(),
                    child: chrome_child,
                    port: new_port,
                    last_used: Instant::now(),
                    target_id: None,
                },
            );
            new_port
        }
    };

    let cfg = json!({
        "port": port,
        "startUrl": start_url,
        "targetId": known_target_id,
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
        spawn_cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let spawn = spawn_cmd.spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            cleanup(&[&helper_path, &script_path, &result_path]);
            return json!({"ok": false, "stdout": "", "exit_code": 1,
                "stderr": format!("node cdp helper spawn failed: {e}")});
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

    if timed_out && !session_liveness_recheck(port, &browser_cfg) {
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
    }

    cleanup(&[&helper_path, &script_path, &result_path]);

    let cdp_error = result_value.get("__cdpError").and_then(|v| v.as_str());
    let ok = exit_code == 0 && !timed_out && cdp_error.is_none();
    let default_debug = || json!({"console": [], "pageErrors": [], "network": [], "performance": null, "gl": {"errors": [], "drawCalls": {}, "errorTotalCount": 0}});
    let mut out = json!({
        "ok": ok,
        "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": t0.elapsed().as_millis() as u64,
    });
    if cdp_error.is_some() {
        out["result"] = Value::Null;
        out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
        return out;
    }
    let launched_real_page = matches!(mode, BrowserMode::Dom) || result_value.get("result").is_some() || result_value.get("elements").is_some();
    match mode {
        BrowserMode::Default => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
        }
        BrowserMode::Capture => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
        }
        BrowserMode::Profile => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["profile"] = result_value.get("profile").cloned().unwrap_or(json!({"timeframe": null, "culprits": []}));
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
            if let Some(p) = &artifact_path {
                out["profile_file"] = json!(p.to_string_lossy());
            }
        }
        BrowserMode::Trace => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["trace"] = result_value.get("trace").cloned().unwrap_or(json!({"wall_us": 0, "gpu_us": 0, "viz_us": 0, "cc_us": 0, "by_category": {}}));
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
            if let Some(p) = &artifact_path {
                out["trace_file"] = json!(p.to_string_lossy());
            }
        }
        BrowserMode::Screenshot => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
            let screenshot_error = result_value.get("screenshot_error").cloned();
            if let Some(p) = &artifact_path {
                if screenshot_error.is_none() {
                    out["screenshot_path"] = json!(p.to_string_lossy());
                }
            }
            if let Some(e) = screenshot_error {
                if !e.is_null() {
                    out["screenshot_error"] = e;
                }
            }
        }
        BrowserMode::Dom => {
            out["selector"] = json!(dom_selector);
            out["match_count"] = result_value.get("match_count").cloned().unwrap_or(json!(0));
            out["elements"] = result_value.get("elements").cloned().unwrap_or(json!([]));
            out["debug"] = result_value.get("debug").cloned().unwrap_or_else(default_debug);
            if let Some(e) = result_value.get("error") {
                if !e.is_null() {
                    out["result"] = json!({ "error": e });
                }
            }
        }
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

fn mode_label(mode: BrowserMode) -> &'static str {
    match mode {
        BrowserMode::Default => "default",
        BrowserMode::Capture => "capture",
        BrowserMode::Profile => "profile",
        BrowserMode::Trace => "trace",
        BrowserMode::Screenshot => "screenshot",
        BrowserMode::Dom => "dom",
    }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

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
