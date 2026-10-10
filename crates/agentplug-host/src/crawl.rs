use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::crawl_lease;
use crate::http_agent::shared_agent;

const CRAWL_HELPER_JS: &str = concat!(
    include_str!("crawl_cdp_tools.mjs"),
    "\n",
    include_str!("crawl_cdp.mjs")
);
pub(crate) const CDP_READY_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) const CDP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CDP_HELPER_BUDGET: Duration = Duration::from_secs(100);
const MAX_WAIT_MS: u64 = 60_000;
const URL_SCHEMES: [&str; 5] = ["http://", "https://", "about:blank", "file://", "data:"];

pub struct ParsedCrawl {
    pub engine: Option<String>,
    pub steps: Vec<Value>,
}

pub struct HelperRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub result: Option<Value>,
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        kill_process_tree(self.0.id());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::windowless::apply_windowless(&mut cmd);
        let _ = cmd.status();
    }
}

/// Closes every Chrome left running on the gm-owned crawl profile by an earlier
/// call. The profile path is the marker: user Chrome never carries it.
fn profile_markers(profile: &Path) -> Vec<String> {
    let raw = profile.display().to_string();
    let back = raw.replace('/', "\\");
    let slash = back.replace('\\', "/");
    let mut markers = vec![raw, back, slash];
    markers.sort();
    markers.dedup();
    markers
}

fn escaped(value: &str) -> String {
    value.replace('\'', "''")
}

fn like_any(markers: &[String]) -> String {
    markers
        .iter()
        .map(|marker| format!("$_.CommandLine -like '*{}*'", escaped(marker)))
        .collect::<Vec<_>>()
        .join(" -or ")
}

pub(crate) fn reap_stale_crawl_browsers(profile: &Path) {
    let marker = profile.display().to_string();
    if cfg!(windows) {
        let markers = profile_markers(profile);
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | Where-Object {{ ({}) -and $_.CommandLine -notlike '*--type=*' }} | ForEach-Object {{ $_.ProcessId }}",
            like_any(&markers)
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", &script]).stderr(Stdio::null());
        crate::windowless::apply_windowless(&mut cmd);
        let Ok(output) = cmd.output() else { return };
        for pid in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
        {
            kill_process_tree(pid);
        }
    } else {
        let mut cmd = Command::new("pkill");
        cmd.args(["-f", &format!("--user-data-dir={marker}")]);
        let _ = cmd.status();
    }
}

pub(crate) fn crawl_browser_pid(profile: &Path) -> Option<u32> {
    if !cfg!(windows) {
        return None;
    }
    let raw = profile.display().to_string();
    let back = escaped(&raw.replace('/', "\\"));
    let slash = escaped(&back.replace('\\', "/"));
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | Where-Object {{ ($_.CommandLine -like '*{back}*' -or $_.CommandLine -like '*{slash}*') -and $_.CommandLine -notlike '*--type=*' }} | Select-Object -First 1 -ExpandProperty ProcessId"
    );
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", &script]).stderr(Stdio::null());
    crate::windowless::apply_windowless(&mut cmd);
    let output = cmd.output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()
}

pub(crate) fn process_exists(pid: u32) -> bool {
    if !cfg!(windows) {
        return true;
    }
    let script = format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'yes' }}");
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", &script]).stderr(Stdio::null());
    crate::windowless::apply_windowless(&mut cmd);
    match cmd.output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim() == "yes",
        Err(_) => false,
    }
}

/// Parses a crawl body. An optional first line `engine=cdp|lightpanda` is
/// consumed; every other non-blank, non-`#` line is one step.
pub fn parse_crawl_body(body: &str) -> Result<ParsedCrawl, String> {
    let mut engine = None;
    let mut steps = Vec::new();
    let mut seen_content = false;
    for (index, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        if !seen_content {
            seen_content = true;
            if let Some(value) = line.strip_prefix("engine=") {
                engine = Some(match value.trim() {
                    name @ ("cdp" | "lightpanda") => name.to_string(),
                    other => {
                        return Err(format!(
                            "line {number}: engine must be cdp or lightpanda, got '{other}'"
                        ))
                    }
                });
                continue;
            }
        }
        if is_headless_request(line) {
            return Err(format!(
                "line {number}: headless is not a lightpanda step: engine=lightpanda is headless already, so remove the headless line"
            ));
        }
        steps.push(parse_step(line).map_err(|e| format!("line {number}: {e}"))?);
    }
    finish(engine, steps)
}

fn is_headless_request(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "headless" || lower.starts_with("headless=") || lower.starts_with("--headless")
}

fn finish(engine: Option<String>, steps: Vec<Value>) -> Result<ParsedCrawl, String> {
    if steps.is_empty() {
        return Err(
            "crawl body has no steps: give at least one URL, url=<url>, wait=<ms> or eval=<js>"
                .to_string(),
        );
    }
    Ok(ParsedCrawl { engine, steps })
}

fn parse_step(line: &str) -> Result<Value, String> {
    if let Some(url) = line.strip_prefix("url=") {
        return goto_step(url.trim());
    }
    if URL_SCHEMES.iter().any(|scheme| line.starts_with(scheme)) {
        return goto_step(line);
    }
    if let Some(ms) = line.strip_prefix("wait=") {
        let ms = ms.trim();
        let parsed: u64 = ms
            .parse()
            .map_err(|_| format!("wait= needs whole milliseconds, got '{ms}'"))?;
        if parsed > MAX_WAIT_MS {
            return Err(format!("wait={parsed} exceeds the {MAX_WAIT_MS}ms limit"));
        }
        return Ok(json!({"op": "wait", "ms": parsed}));
    }
    if let Some(code) = line.strip_prefix("eval=") {
        let code = code.trim();
        if code.is_empty() {
            return Err("eval= needs a JavaScript expression".to_string());
        }
        return Ok(json!({"op": "eval", "code": code}));
    }
    Err(format!(
        "unrecognized crawl step '{}' (expected url=<url>, a bare URL, wait=<ms> or eval=<js>)",
        line.chars().take(80).collect::<String>()
    ))
}

fn goto_step(url: &str) -> Result<Value, String> {
    if !URL_SCHEMES.iter().any(|scheme| url.starts_with(scheme)) {
        return Err(format!(
            "'{url}' is not a URL (http://, https://, file://, data: or about:blank)"
        ));
    }
    Ok(json!({"op": "goto", "url": url}))
}

pub struct CdpCrawl {
    pub session: Option<String>,
    pub steps: Vec<Value>,
    pub headful: bool,
}

const TOOL_STEP_GRAMMAR: &str ="snapshot, click=<uid>, dblclick=<uid>, hover=<uid>, click_at=<x>,<y>, fill=<uid> <value>, type=<text>, press=<key>, press_uid=<uid> <key>, upload=<uid> <path>[;<path>], wait_for=<text>, reload, back, forward, console, network[=<reqid>], dialog=accept|dismiss, screenshot=<path>, screenshot_full=<path>, screenshot_uid=<uid> <path>, trace_start, trace_stop[=<path>]";

pub fn parse_cdp_crawl_body(body: &str) -> Result<CdpCrawl, String> {
    let mut session = None;
    let mut headful = env_flag("GM_CRAWL_CDP_HEADFUL");
    let mut steps = Vec::new();
    for (index, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        if steps.is_empty() {
            if let Some(value) = line.strip_prefix("engine=") {
                if value.trim() != "cdp" {
                    return Err(format!(
                        "line {number}: engine must be cdp here, got '{}'",
                        value.trim()
                    ));
                }
                continue;
            }
            if let Some(value) = line.strip_prefix("session=") {
                if session.is_some() {
                    return Err(format!("line {number}: session= is given twice"));
                }
                session =
                    Some(session_name(value.trim()).map_err(|e| format!("line {number}: {e}"))?);
                continue;
            }
            if is_headful_request(line) {
                headful = true;
                continue;
            }
            if is_headless_request(line) {
                headful = false;
                continue;
            }
        } else if is_headful_request(line) || is_headless_request(line) {
            return Err(format!(
                "line {number}: headless/headful is a leading directive and must come before the first step"
            ));
        }
        steps.push(parse_cdp_step(line).map_err(|e| format!("line {number}: {e}"))?);
    }
    if steps.is_empty() {
        return Err(
            "crawl body has no steps: give at least one URL, url=<url>, wait=<ms>, eval=<js> or a browser step"
                .to_string(),
        );
    }
    Ok(CdpCrawl {
        session,
        steps,
        headful,
    })
}

fn is_headful_request(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "headful" || lower.starts_with("headful=") || lower.starts_with("--headful")
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name)
        .map(|value| {
            let value = value.to_string_lossy().to_ascii_lowercase();
            !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
        })
        .unwrap_or(false)
}

fn session_name(name: &str) -> Result<String, String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(name.to_string())
    } else {
        Err(format!(
            "session name must be 1-64 letters, digits, '-' or '_', got '{name}'"
        ))
    }
}

fn parse_cdp_step(line: &str) -> Result<Value, String> {
    let legacy = line.starts_with("url=")
        || line.starts_with("wait=")
        || line.starts_with("eval=")
        || URL_SCHEMES.iter().any(|scheme| line.starts_with(scheme));
    if legacy {
        return parse_step(line);
    }
    parse_tool_step(line)
}

fn parse_tool_step(line: &str) -> Result<Value, String> {
    let (name, arg) = match line.split_once('=') {
        Some((name, arg)) => (name.trim(), Some(arg.trim())),
        None => (line.trim(), None),
    };
    let step = match (name, arg) {
        ("snapshot" | "console" | "reload" | "back" | "forward" | "trace_start", None) => {
            json!({ "op": name })
        }
        ("network", None) => json!({ "op": "network" }),
        ("network", Some(id)) => json!({ "op": "network", "reqid": require_number(id, "reqid")? }),
        ("trace_stop", None) => json!({ "op": "trace_stop" }),
        ("trace_stop", Some(path)) => {
            json!({ "op": "trace_stop", "path": non_empty(path, "trace_stop path")? })
        }
        ("click" | "dblclick" | "hover", Some(uid)) => {
            json!({ "op": name, "uid": require_uid(uid)? })
        }
        ("click_at", Some(point)) => {
            let (x, y) = point
                .split_once(',')
                .ok_or_else(|| "click_at needs x,y in CSS pixels".to_string())?;
            json!({ "op": "click_at", "x": require_coordinate(x)?, "y": require_coordinate(y)? })
        }
        ("fill", Some(rest)) => {
            let (uid, value) = split_first_word(rest);
            json!({ "op": "fill", "uid": require_uid(uid)?, "value": value })
        }
        ("type", Some(text)) => json!({ "op": "type", "text": non_empty(text, "type text")? }),
        ("press", Some(key)) => json!({ "op": "press", "key": non_empty(key, "press key")? }),
        ("press_uid", Some(rest)) => {
            let (uid, key) = split_first_word(rest);
            json!({ "op": "press", "uid": require_uid(uid)?, "key": non_empty(key, "press key")? })
        }
        ("upload", Some(rest)) => {
            let (uid, paths) = split_first_word(rest);
            let files: Vec<String> = paths
                .split(';')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            if files.is_empty() {
                return Err("upload needs a file path after the uid".to_string());
            }
            json!({ "op": "upload", "uid": require_uid(uid)?, "paths": files })
        }
        ("wait_for", Some(text)) => {
            json!({ "op": "wait_for", "text": non_empty(text, "wait_for text")? })
        }
        ("dialog", Some(action)) if action == "accept" || action == "dismiss" => {
            json!({ "op": "dialog", "action": action })
        }
        ("screenshot", Some(path)) => {
            json!({ "op": "screenshot", "path": non_empty(path, "screenshot path")? })
        }
        ("screenshot_full", Some(path)) => {
            json!({ "op": "screenshot", "path": non_empty(path, "screenshot path")?, "full": true })
        }
        ("screenshot_uid", Some(rest)) => {
            let (uid, path) = split_first_word(rest);
            json!({ "op": "screenshot", "uid": require_uid(uid)?, "path": non_empty(path, "screenshot path")? })
        }
        _ => {
            return Err(format!(
                "unrecognized crawl step '{}' (expected url=<url>, a bare URL, wait=<ms>, eval=<js>, or one of: {TOOL_STEP_GRAMMAR})",
                line.chars().take(80).collect::<String>()
            ))
        }
    };
    Ok(step)
}

fn split_first_word(rest: &str) -> (&str, &str) {
    match rest.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim_start()),
        None => (rest, ""),
    }
}

fn require_uid(uid: &str) -> Result<String, String> {
    if !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()) {
        Ok(uid.to_string())
    } else {
        Err(format!(
            "uid must be the number printed in a snapshot line, got '{uid}'"
        ))
    }
}

fn require_number(value: &str, what: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{what} must be a whole number, got '{value}'"))
}

fn require_coordinate(value: &str) -> Result<f64, String> {
    match value.trim().parse::<f64>() {
        Ok(number) if number.is_finite() => Ok(number),
        _ => Err(format!(
            "click_at coordinates must be numbers, got '{value}'"
        )),
    }
}

fn non_empty(value: &str, what: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("{what} is empty"))
    } else {
        Ok(value.to_string())
    }
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path_var).find_map(|dir| {
        names
            .iter()
            .map(|n| dir.join(n))
            .find(|candidate| candidate.is_file())
    })
}

pub fn free_local_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|e| format!("could not reserve a local port: {e}"))
}

pub fn endpoint_ready(
    port: u16,
    deadline: Instant,
    interval: Duration,
    require_targets: bool,
) -> bool {
    loop {
        if endpoint_state(port, require_targets).is_ok() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(interval.min(deadline - now));
    }
}

fn endpoint_state(port: u16, require_targets: bool) -> Result<(), String> {
    let version = endpoint_get(port, "/json/version")?;
    if !version.contains("webSocketDebuggerUrl") {
        return Err(format!(
            "/json/version answered without webSocketDebuggerUrl: {}",
            first_chars(&version, 120)
        ));
    }
    if require_targets {
        let list = endpoint_get(port, "/json/list")?;
        match serde_json::from_str::<Value>(&list) {
            Ok(Value::Array(_)) => {}
            _ => {
                return Err(format!(
                    "/json/list answered without a target array: {}",
                    first_chars(&list, 120)
                ))
            }
        }
    }
    Ok(())
}

fn endpoint_get(port: u16, path: &str) -> Result<String, String> {
    shared_agent()
        .get(&format!("http://127.0.0.1:{port}{path}"))
        .timeout(Duration::from_millis(800))
        .call()
        .map_err(|e| format!("{path} did not answer: {e}"))
        .and_then(|resp| {
            resp.into_string()
                .map_err(|e| format!("{path} replied with an unreadable body: {e}"))
        })
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

pub fn endpoint_failure(port: u16) -> String {
    match endpoint_state(port, true) {
        Ok(()) => "the endpoint answered but carried no usable target".to_string(),
        Err(e) => e,
    }
}

fn windows_chrome_candidates() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
        .collect();
    if bases.is_empty() {
        bases.push(PathBuf::from(r"C:\Program Files"));
    }
    bases
        .iter()
        .flat_map(|base| {
            [
                base.join("Google").join("Chrome").join("Application").join("chrome.exe"),
                base.join("Chromium").join("Application").join("chrome.exe"),
            ]
        })
        .collect()
}

fn macos_chrome_candidates() -> Vec<PathBuf> {
    let mut app_dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        app_dirs.push(home.join("Applications"));
    }
    app_dirs
        .iter()
        .flat_map(|dir| {
            [
                dir.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
                dir.join("Chromium.app/Contents/MacOS/Chromium"),
            ]
        })
        .collect()
}

fn linux_chrome_candidates() -> Vec<PathBuf> {
    [
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/opt/google/chrome/chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

pub(crate) fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GM_BROWSER_CHROME_PATH")
        .or_else(|| std::env::var_os("CHROME_PATH"))
        .map(PathBuf::from)
        .filter(|p| p.exists())
    {
        return Some(p);
    }
    let candidates = if cfg!(windows) {
        windows_chrome_candidates()
    } else if cfg!(target_os = "macos") {
        macos_chrome_candidates()
    } else {
        linux_chrome_candidates()
    };
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .or_else(|| {
            ["chrome", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]
                .iter()
                .find_map(|name| find_on_path(name))
        })
}

pub type LightpandaEngine = fn(&Path, &str) -> Value;

static LIGHTPANDA_ENGINE: std::sync::OnceLock<LightpandaEngine> = std::sync::OnceLock::new();

/// Registers the native lightpanda engine that the `host_lightpanda_crawl`
/// import calls. The runner registers it once at startup, because the engine
/// crate depends on this crate and cannot be linked here.
pub fn set_lightpanda_engine(engine: LightpandaEngine) {
    let _ = LIGHTPANDA_ENGINE.set(engine);
}

pub fn lightpanda_crawl(cwd: &Path, body: &str) -> Value {
    match LIGHTPANDA_ENGINE.get() {
        Some(engine) => engine(cwd, body),
        None => crawl_error_reply(
            "lightpanda",
            true,
            Instant::now(),
            "the lightpanda engine is not registered in this runner".to_string(),
        ),
    }
}

fn read_pipe<R: Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

/// Runs the Node CDP helper against an endpoint on `port`. The script is fed on
/// stdin; the config travels in GM_CRAWL_CONFIG. The child is killed when the
/// budget runs out.
pub fn run_helper(
    port: u16,
    target_id: Option<&str>,
    steps: &[Value],
    budget: Duration,
    browser_session: bool,
) -> Result<HelperRun, String> {
    run_helper_with(port, target_id, steps, budget, browser_session, json!({}))
}

fn run_helper_with(
    port: u16,
    target_id: Option<&str>,
    steps: &[Value],
    budget: Duration,
    browser_session: bool,
    extra: Value,
) -> Result<HelperRun, String> {
    let node = find_on_path("node").ok_or("node is required on PATH to run the crawl helper")?;
    let mut config = json!({
        "port": port,
        "targetId": target_id,
        "browserSession": browser_session,
        "steps": steps,
        "pageTimeoutMs": 30000,
        "textLimit": 20000,
    });
    if let (Some(base), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    let mut cmd = Command::new(node);
    cmd.arg("--input-type=module")
        .env("GM_CRAWL_CONFIG", config.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::windowless::apply_windowless(&mut cmd);
    let mut child = KillOnDrop(cmd.spawn().map_err(|e| format!("could not start node: {e}"))?);
    if let Some(mut stdin) = child.0.stdin.take() {
        let _ = stdin.write_all(CRAWL_HELPER_JS.as_bytes());
    }
    let out_pipe: Option<ChildStdout> = child.0.stdout.take();
    let err_pipe: Option<ChildStderr> = child.0.stderr.take();
    let out_reader = std::thread::spawn(move || read_pipe(out_pipe));
    let err_reader = std::thread::spawn(move || read_pipe(err_pipe));
    let (exit_code, timed_out) = match child.0.wait_timeout(budget) {
        Ok(Some(status)) => (status.code(), false),
        Ok(None) | Err(_) => {
            let _ = child.0.kill();
            let _ = child.0.wait();
            (None, true)
        }
    };
    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    let result = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    Ok(HelperRun {
        stdout,
        stderr,
        exit_code,
        timed_out,
        result,
    })
}

pub fn crawl_error_reply(engine: &str, headless: bool, started: Instant, error: String) -> Value {
    json!({
        "ok": false,
        "engine": engine,
        "headless": headless,
        "stdout": "",
        "stderr": "",
        "exit_code": null,
        "duration_ms": started.elapsed().as_millis() as u64,
        "timed_out": false,
        "target_id": null,
        "pages": [],
        "error": error,
    })
}

pub fn crawl_reply_from_run(
    engine: &str,
    headless: bool,
    started: Instant,
    run: HelperRun,
) -> Value {
    let result = run.result.clone().unwrap_or(Value::Null);
    let helper_ok = result.get("ok").and_then(Value::as_bool) == Some(true);
    let ok = !run.timed_out && run.exit_code == Some(0) && helper_ok;
    let error = if ok {
        Value::Null
    } else if run.timed_out {
        json!("crawl helper exceeded its time budget and was killed")
    } else {
        match result.get("error").filter(|e| !e.is_null()) {
            Some(e) => e.clone(),
            None => json!(format!(
                "crawl helper exited with {:?} without a result: {}",
                run.exit_code,
                run.stderr.trim()
            )),
        }
    };
    let target_id = result.get("targetId").cloned().unwrap_or(Value::Null);
    let pages = result.get("pages").cloned().unwrap_or_else(|| json!([]));
    json!({
        "ok": ok,
        "engine": engine,
        "headless": headless,
        "stdout": run.stdout,
        "stderr": run.stderr,
        "exit_code": run.exit_code,
        "duration_ms": started.elapsed().as_millis() as u64,
        "timed_out": run.timed_out,
        "target_id": target_id,
        "pages": pages,
        "error": error,
    })
}

/// Headful Chrome over CDP on the single browser shared by this project root.
/// The call holds a lease on it and releases the lease on return.
pub fn crawl_cdp(cwd: &Path, body: &str) -> Value {
    let started = Instant::now();
    let parsed = match parse_cdp_crawl_body(body) {
        Ok(parsed) => parsed,
        Err(e) => return crawl_error_reply("cdp", false, started, e),
    };
    let headless = !parsed.headful;
    let agent = crawl_lease::next_agent_id();
    let (port, _) = match crawl_lease::acquire(cwd, &agent, parsed.headful) {
        Ok(attached) => attached,
        Err(e) => return crawl_error_reply("cdp", headless, started, e),
    };
    let run = run_helper_with(
        port,
        None,
        &parsed.steps,
        CDP_HELPER_BUDGET,
        false,
        cdp_run_config(cwd, parsed.session.as_deref()),
    );
    let reply = match run {
        Ok(run) => crawl_reply_from_run("cdp", headless, started, run),
        Err(e) => crawl_error_reply("cdp", headless, started, e),
    };
    crawl_lease::release(cwd, &agent);
    reply
}

fn cdp_run_config(cwd: &Path, session: Option<&str>) -> Value {
    let mut config = json!({
        "cwd": cwd.display().to_string(),
        "watchdogMs": CDP_HELPER_BUDGET.as_millis() as u64 - 10_000,
    });
    if let Some(name) = session {
        let file = cwd
            .join(".gm")
            .join("crawl-cdp-sessions")
            .join(format!("{name}.json"));
        config["session"] = json!({ "name": name, "file": file.display().to_string() });
    }
    config
}
