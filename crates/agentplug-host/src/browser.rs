use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::browser_lease;
use crate::http_agent::shared_agent;

const BROWSER_HELPER_JS: &str = concat!(
    include_str!("browser_cdp_tools.mjs"),
    "\n",
    include_str!("browser_cursor.mjs"),
    "\n",
    include_str!("browser_debugger.mjs"),
    "\n",
    include_str!("browser_editor.mjs"),
    "\n",
    include_str!("browser_cdp.mjs")
);
pub(crate) const CDP_READY_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) const CDP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CDP_HELPER_BUDGET: Duration = Duration::from_secs(100);
const MAX_WAIT_MS: u64 = 60_000;
const MAX_EVENT_LIMIT: u64 = 10_000;
const DEFAULT_EVENT_LIMIT: u64 = 200;
const MAX_CDP_PARAMS_LINES: usize = 64;
type StepLines<'a> = std::iter::Peekable<std::iter::Enumerate<std::str::Lines<'a>>>;
const URL_SCHEMES: [&str; 5] = ["http://", "https://", "about:blank", "file://", "data:"];

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

#[cfg(unix)]
fn platform_kill(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn platform_kill(_pid: u32) {}

// Asks a process to exit cleanly. SIGTERM on unix; elsewhere there is no such request.
#[cfg(unix)]
pub(crate) fn request_exit(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
}

#[cfg(not(unix))]
pub(crate) fn request_exit(_pid: u32) -> bool {
    false
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
    } else {
        platform_kill(pid);
    }
}

fn gm_profile_command_line_markers(profile: &Path) -> Vec<String> {
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

pub(crate) fn reap_stale_browsers(root: &Path) -> Result<(), String> {
    let profile = browser_lease::profile_dir(root);
    if cfg!(windows) {
        let markers = gm_profile_command_line_markers(&profile);
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | Where-Object {{ ({}) -and $_.CommandLine -notlike '*--type=*' }} | ForEach-Object {{ $_.ProcessId }}",
            like_any(&markers)
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", &script]).stderr(Stdio::null());
        crate::windowless::apply_windowless(&mut cmd);
        let Ok(output) = cmd.output() else { return Ok(()) };
        for pid in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
        {
            kill_process_tree(pid);
            browser_lease::log_browser(root, "browser-reaped", pid, "reason=profile-sweep");
        }
        Ok(())
    } else {
        let marker = format!("--user-data-dir={}", profile.display());
        let output = Command::new("ps")
            .args(["-axww", "-o", "pid=,args="])
            .output()
            .map_err(|e| format!("could not run ps to reap stale browser browsers: {e}"))?;
        let listed: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| marker_ends_at_boundary(line, &marker))
            .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
            .collect();
        for pid in listed {
            request_exit(pid);
            browser_lease::log_browser(root, "browser-reaped", pid, "reason=profile-sweep");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn unix_browser_pid(profile: &Path) -> Option<u32> {
    let marker = format!("--user-data-dir={}", profile.display());
    let mut cmd = Command::new("ps");
    cmd.args(["-axww", "-o", "pid=,args="]).stderr(Stdio::null());
    let output = cmd.output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            !line.contains("--type=") && marker_ends_at_boundary(line, &marker) && !is_shell_launcher(line)
        })
        .find_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
}

#[cfg(unix)]
fn is_shell_launcher(line: &str) -> bool {
    line.split_whitespace()
        .nth(1)
        .and_then(|exe| exe.rsplit('/').next())
        .is_some_and(|name| matches!(name, "sh" | "bash" | "dash" | "zsh"))
}

fn marker_ends_at_boundary(line: &str, marker: &str) -> bool {
    line.match_indices(marker).any(|(at, _)| {
        line[at + marker.len()..]
            .chars()
            .next()
            .map_or(true, char::is_whitespace)
    })
}

#[cfg(not(unix))]
fn unix_browser_pid(_profile: &Path) -> Option<u32> {
    None
}

pub(crate) fn browser_pid(profile: &Path) -> Option<u32> {
    if !cfg!(windows) {
        return unix_browser_pid(profile);
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

#[cfg(unix)]
fn unix_process_exists(pid: u32) -> bool {
    // Signal 0 only probes. EPERM still means the process exists, owned by someone else.
    let probed = unsafe { libc::kill(pid as libc::pid_t, 0) };
    probed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn unix_process_exists(_pid: u32) -> bool {
    true
}

pub(crate) fn process_exists(pid: u32) -> bool {
    if !cfg!(windows) {
        return unix_process_exists(pid);
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

fn is_headless_request(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "headless" || lower.starts_with("headless=") || lower.starts_with("--headless")
}

fn parse_step(line: &str) -> Result<Value, String> {
    if let Some(url) = line.strip_prefix("url=") {
        return goto_step(url.trim());
    }
    if URL_SCHEMES.iter().any(|scheme| line.starts_with(scheme)) {
        return goto_step(line);
    }
    if let Some(ms) = line.strip_prefix("wait=") {
        return parse_wait_ms(ms.trim());
    }
    if let Some(code) = line.strip_prefix("eval=") {
        let code = code.trim();
        if code.is_empty() {
            return Err("eval= needs a JavaScript expression".to_string());
        }
        return Ok(json!({"op": "eval", "code": code}));
    }
    Err(format!(
        "unrecognized browser step '{}' (expected url=<url>, a bare URL, wait=<ms> or eval=<js>)",
        line.chars().take(80).collect::<String>()
    ))
}

fn parse_wait_ms(ms: &str) -> Result<Value, String> {
    let parsed: u64 = ms
        .parse()
        .map_err(|_| format!("wait needs whole milliseconds, got '{ms}'"))?;
    if parsed > MAX_WAIT_MS {
        return Err(format!("wait={parsed} exceeds the {MAX_WAIT_MS}ms limit"));
    }
    Ok(json!({"op": "wait", "ms": parsed}))
}

fn goto_step(url: &str) -> Result<Value, String> {
    if !URL_SCHEMES.iter().any(|scheme| url.starts_with(scheme)) {
        return Err(format!(
            "'{url}' is not a URL (http://, https://, file://, data: or about:blank)"
        ));
    }
    Ok(json!({"op": "goto", "url": url}))
}

#[derive(Default)]
pub struct Expectation {
    pub tabs: Option<u64>,
    pub reuse: Option<String>,
    pub open: Option<u64>,
}

impl Expectation {
    fn is_empty(&self) -> bool {
        self.tabs.is_none() && self.reuse.is_none() && self.open.is_none()
    }
}

pub struct CdpBrowser {
    pub session: Option<String>,
    pub steps: Vec<Value>,
    pub headful: bool,
    pub expect: Expectation,
    pub timeout: Option<Duration>,
}

// timeout=<ms> bounds one call's helper run. Default CDP_HELPER_BUDGET.
const CDP_TIMEOUT_MIN_MS: u64 = 1_000;
const CDP_TIMEOUT_MAX_MS: u64 = 300_000;

const TOOL_STEP_GRAMMAR: &str ="snapshot, click=<uid>, dblclick=<uid>, hover=<uid>, click_at=<x>,<y>, fill=<uid> <value>, type=<text>, press=<key>, press_uid=<uid> <key>, upload=<uid> <path>[;<path>], wait_for=<text>, reload, back, forward, console, network[=<reqid>], dialog=accept|dismiss, screenshot=<path>, screenshot_full=<path>, screenshot_uid=<uid> <path>, trace_start, trace_stop[=<path>], json <object with an op field>, cdp <Domain.method> (+ JSON params on the next line), events <Domain.event> [where <field>~<value>] [limit=N], wait <ms>";

pub fn parse_cdp_browser_body(body: &str) -> Result<CdpBrowser, String> {
    let mut session = None;
    let headful = true;
    let mut expect = Expectation::default();
    let mut timeout = None;
    let mut steps = Vec::new();
    let mut lines = body.lines().enumerate().peekable();
    while let Some((index, raw)) = lines.next() {
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
            if let Some(value) = line
                .strip_prefix("session=")
                .or_else(|| line.strip_prefix("sessionId="))
            {
                if session.is_some() {
                    return Err(format!(
                        "line {number}: session= is given twice; session= and sessionId= name the same session"
                    ));
                }
                session =
                    Some(session_name(value.trim()).map_err(|e| format!("line {number}: {e}"))?);
                continue;
            }
            if let Some(value) = line.strip_prefix("expect=") {
                let more = parse_expect(value).map_err(|e| format!("line {number}: {e}"))?;
                expect.tabs = more.tabs.or(expect.tabs);
                expect.reuse = more.reuse.or(expect.reuse);
                expect.open = more.open.or(expect.open);
                continue;
            }
            if let Some(value) = line.strip_prefix("timeout=") {
                let ms = require_number(value.trim(), "timeout")
                    .map_err(|e| format!("line {number}: {e}"))?;
                if !(CDP_TIMEOUT_MIN_MS..=CDP_TIMEOUT_MAX_MS).contains(&ms) {
                    return Err(format!(
                        "line {number}: timeout must be {CDP_TIMEOUT_MIN_MS}-{CDP_TIMEOUT_MAX_MS} ms, got {ms}"
                    ));
                }
                timeout = Some(Duration::from_millis(ms));
                continue;
            }
            if is_headful_request(line) {
                continue;
            }
            if is_headless_request(line) {
                return Err(format!(
                    "line {number}: headless is refused: engine=cdp runs a headful browser"
                ));
            }
        } else if is_headful_request(line) || is_headless_request(line) {
            return Err(format!(
                "line {number}: headless/headful is a leading directive and must come before the first step"
            ));
        }
        steps.push(parse_cdp_step(line, &mut lines).map_err(|e| format!("line {number}: {e}"))?);
    }
    if steps.is_empty() {
        return Err(
            "browser body has no steps: give at least one URL, url=<url>, wait=<ms>, eval=<js> or a browser step"
                .to_string(),
        );
    }
    Ok(CdpBrowser {
        session,
        steps,
        headful,
        expect,
        timeout,
    })
}

fn is_headful_request(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "headful" || lower.starts_with("headful=") || lower.starts_with("--headful")
}

// expect=tabs:N reuse:<session> open:N (or key=value). Each key is checked against the lease's
// tab list before any step runs, so a difference fails the call before the browser is touched.
fn parse_expect(value: &str) -> Result<Expectation, String> {
    let mut expect = Expectation::default();
    for token in value.split_whitespace() {
        let (key, arg) = token
            .split_once(|c: char| c == '=' || c == ':')
            .ok_or_else(|| format!("expect: '{token}' needs key=value or key:value"))?;
        match key {
            "tabs" => expect.tabs = Some(require_number(arg, "expect tabs")?),
            "open" => expect.open = Some(require_number(arg, "expect open")?),
            "reuse" => expect.reuse = Some(session_name(arg)?),
            other => {
                return Err(format!(
                    "expect: unknown key '{other}' (use tabs, reuse or open)"
                ))
            }
        }
    }
    Ok(expect)
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

fn keyword_arg<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    match line.split_once(char::is_whitespace) {
        Some((head, rest)) if head == keyword => {
            let rest = rest.trim_start();
            if rest.is_empty() {
                None
            } else {
                Some(rest)
            }
        }
        _ => None,
    }
}

fn parse_cdp_step(line: &str, lines: &mut StepLines<'_>) -> Result<Value, String> {
    if line == "close" {
        return Ok(json!({"op": "close"}));
    }
    if let Some(arg) = keyword_arg(line, "cdp") {
        return parse_raw_cdp_step(arg, lines);
    }
    if let Some(arg) = keyword_arg(line, "events") {
        return parse_events_step(arg);
    }
    if let Some(arg) = keyword_arg(line, "wait") {
        return parse_wait_ms(arg.trim_end());
    }
    let legacy = line.starts_with("url=")
        || line.starts_with("wait=")
        || line.starts_with("eval=")
        || URL_SCHEMES.iter().any(|scheme| line.starts_with(scheme));
    if legacy {
        return parse_step(line);
    }
    parse_tool_step(line)
}

fn parse_raw_cdp_step(method: &str, lines: &mut StepLines<'_>) -> Result<Value, String> {
    let method = require_cdp_method(method, "cdp")?;
    let params = read_cdp_params(lines)?;
    Ok(json!({"op": "cdp", "method": method, "params": params}))
}

fn require_cdp_method<'a>(method: &'a str, what: &str) -> Result<&'a str, String> {
    let parts: Vec<&str> = method.split('.').collect();
    let valid = parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    if valid {
        Ok(method)
    } else {
        Err(format!(
            "{what} needs one Domain.method name, got '{method}'"
        ))
    }
}

fn read_cdp_params(lines: &mut StepLines<'_>) -> Result<Value, String> {
    let starts_object = match lines.peek() {
        Some((_, raw)) => raw.trim_start().starts_with('{'),
        None => false,
    };
    if !starts_object {
        return Ok(json!({}));
    }
    let mut text = String::new();
    let mut consumed = 0usize;
    while let Some((index, raw)) = lines.next() {
        consumed += 1;
        text.push_str(raw);
        match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => return Ok(value),
            Ok(value) => {
                return Err(format!(
                    "line {}: cdp params must be a JSON object, got {value}",
                    index + 1
                ))
            }
            Err(_) if consumed < MAX_CDP_PARAMS_LINES => {}
            Err(e) => {
                return Err(format!(
                    "line {}: cdp params are not valid JSON: {e}",
                    index + 1
                ))
            }
        }
    }
    Err("cdp params started with '{' but the JSON object never closed".to_string())
}

fn parse_events_step(arg: &str) -> Result<Value, String> {
    let mut tokens = arg.split_whitespace().peekable();
    let name = tokens
        .next()
        .ok_or_else(|| "events needs an event name, e.g. events Debugger.scriptParsed".to_string())?;
    let event = require_cdp_method(name, "events")?;
    let mut filter = None;
    let mut limit = None;
    while let Some(token) = tokens.next() {
        if token == "where" {
            if filter.is_some() {
                return Err("events: where is given twice".to_string());
            }
            let mut parts: Vec<&str> = Vec::new();
            loop {
                let is_limit = match tokens.peek() {
                    Some(next) => next.starts_with("limit="),
                    None => false,
                };
                if is_limit {
                    break;
                }
                match tokens.next() {
                    Some(part) => parts.push(part),
                    None => break,
                }
            }
            if parts.is_empty() {
                return Err(
                    "events: where needs a filter, e.g. where url~mapspinner".to_string()
                );
            }
            filter = Some(parse_event_filter(&parts.join(" "))?);
        } else if let Some(value) = token.strip_prefix("limit=") {
            if limit.is_some() {
                return Err("events: limit= is given twice".to_string());
            }
            limit = Some(require_event_limit(value)?);
        } else {
            return Err(format!(
                "events: unexpected '{token}' (expected where <filter> or limit=N)"
            ));
        }
    }
    let mut step = json!({"op": "events", "event": event, "limit": limit.unwrap_or(DEFAULT_EVENT_LIMIT)});
    if let Some(filter) = filter {
        step["where"] = filter;
    }
    Ok(step)
}

fn parse_event_filter(expr: &str) -> Result<Value, String> {
    let (token, op, at) = [("!=", "not_equals"), ("~", "contains"), ("=", "equals")]
        .iter()
        .filter_map(|(token, op)| expr.find(*token).map(|at| (*token, *op, at)))
        .min_by_key(|(_, _, at)| *at)
        .ok_or_else(|| {
            format!("events: where needs <field>~<value>, <field>=<value> or <field>!=<value>, got '{expr}'")
        })?;
    let field = expr[..at].trim();
    let value = expr[at + token.len()..].trim();
    let field_ok = !field.is_empty()
        && field.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    if !field_ok {
        return Err(format!(
            "events: where field must be a dotted path of names, got '{field}'"
        ));
    }
    if value.is_empty() {
        return Err(format!("events: where needs a value after '{token}'"));
    }
    Ok(json!({"field": field, "op": op, "value": value}))
}

fn require_event_limit(value: &str) -> Result<u64, String> {
    let parsed: u64 = value
        .parse()
        .map_err(|_| format!("events: limit must be a whole number, got '{value}'"))?;
    if parsed < 1 || parsed > MAX_EVENT_LIMIT {
        return Err(format!(
            "events: limit must be 1..{MAX_EVENT_LIMIT}, got {parsed}"
        ));
    }
    Ok(parsed)
}

// json <object>: any helper op (lanes 1, 5) as one JSON step, passed to the helper with its op unchanged.
fn parse_json_step(text: &str) -> Result<Value, String> {
    let step: Value = serde_json::from_str(text)
        .map_err(|e| format!("json step must be a JSON object: {e}"))?;
    match step.get("op").and_then(Value::as_str) {
        Some(op) if !op.is_empty() && step.is_object() => Ok(step),
        _ => Err("json step needs an \"op\" string, e.g. json {\"op\":\"clean_html\"}".to_string()),
    }
}

fn parse_tool_step(line: &str) -> Result<Value, String> {
    if let Some(rest) = line.strip_prefix("json ").or_else(|| line.strip_prefix("json=")) {
        return parse_json_step(rest.trim());
    }
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
                "unrecognized browser step '{}' (expected url=<url>, a bare URL, wait=<ms>, eval=<js>, or one of: {TOOL_STEP_GRAMMAR})",
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

#[cfg(windows)]
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

#[cfg(target_os = "macos")]
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

#[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(windows)]
    let candidates = windows_chrome_candidates();
    #[cfg(target_os = "macos")]
    let candidates = macos_chrome_candidates();
    #[cfg(not(any(windows, target_os = "macos")))]
    let candidates = linux_chrome_candidates();
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .or_else(|| {
            ["chrome", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]
                .iter()
                .find_map(|name| find_on_path(name))
        })
}

fn read_pipe<R: Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

pub fn run_helper(
    port: u16,
    target_id: Option<&str>,
    steps: &[Value],
    budget: Duration,
) -> Result<HelperRun, String> {
    run_helper_with(port, target_id, steps, budget, json!({}))
}

fn run_helper_with(
    port: u16,
    target_id: Option<&str>,
    steps: &[Value],
    budget: Duration,
    extra: Value,
) -> Result<HelperRun, String> {
    let node = find_on_path("node").ok_or("node is required on PATH to run the browser helper")?;
    let mut config = json!({
        "port": port,
        "targetId": target_id,
        "steps": steps,
        "pageTimeoutMs": 30000,
        "textLimit": 20000,
    });
    if let (Some(base), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    let partial_path = config
        .get("partialFile")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let working_dir = config
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    if let Some(base) = config.as_object_mut() {
        base.insert("cwd".into(), json!(working_dir.to_string_lossy()));
    }
    let mut cmd = Command::new(node);
    cmd.arg("--input-type=module")
        .current_dir(&working_dir)
        .env("GM_BROWSER_CONFIG", config.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::windowless::apply_windowless(&mut cmd);
    let mut child = KillOnDrop(cmd.spawn().map_err(|e| format!("could not start node: {e}"))?);
    if let Some(mut stdin) = child.0.stdin.take() {
        let _ = stdin.write_all(BROWSER_HELPER_JS.as_bytes());
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
    let final_line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    let partial = partial_path.and_then(|path| {
        let text = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);
        text.and_then(|text| serde_json::from_str::<Value>(&text).ok())
    });
    let result = final_line.or(partial);
    Ok(HelperRun {
        stdout,
        stderr,
        exit_code,
        timed_out,
        result,
    })
}

pub fn browser_error_reply(engine: &str, headless: bool, started: Instant, error: String) -> Value {
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

pub fn browser_reply_from_run(
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
        json!("browser helper exceeded its time budget and was killed")
    } else {
        match result.get("error").filter(|e| !e.is_null()) {
            Some(e) => e.clone(),
            None => json!(format!(
                "browser helper exited with {:?} without a result: {}",
                run.exit_code,
                run.stderr.trim()
            )),
        }
    };
    let target_id = result.get("targetId").cloned().unwrap_or(Value::Null);
    let pages = result.get("pages").cloned().unwrap_or_else(|| json!([]));
    let steps = result.get("steps").cloned().unwrap_or_else(|| json!([]));
    let tab = result.get("tab").cloned().unwrap_or(Value::Null);
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
        "steps": steps,
        "tab": tab,
        "error": error,
    })
}

pub fn browser_cdp(cwd: &Path, body: &str) -> Value {
    let started = Instant::now();
    let parsed = match parse_cdp_browser_body(body) {
        Ok(parsed) => parsed,
        Err(e) => return browser_error_reply("cdp", false, started, e),
    };
    let headless = !parsed.headful;
    let agent = browser_lease::next_agent_id();
    let (port, _) = match browser_lease::acquire(cwd, &agent, parsed.headful) {
        Ok(attached) => attached,
        Err(e) => return browser_error_reply("cdp", headless, started, e),
    };
    let expect = match check_expectations(port, cwd, parsed.session.as_deref(), &parsed.expect) {
        Ok(report) => report,
        Err(e) => {
            browser_lease::release(cwd, &agent);
            let mut reply = browser_error_reply("cdp", headless, started, e.clone());
            if let Some(obj) = reply.as_object_mut() {
                obj.insert("expect".to_string(), json!({ "ok": false, "error": e }));
                obj.insert("steps".to_string(), json!([]));
            }
            return reply;
        }
    };
    let attendance = browser_lease::attend_while_running(cwd);
    let budget = parsed.timeout.unwrap_or(CDP_HELPER_BUDGET);
    let run = run_helper_with(
        port,
        None,
        &parsed.steps,
        budget,
        cdp_run_config(cwd, parsed.session.as_deref(), budget),
    );
    drop(attendance);
    let mut reply = match run {
        Ok(run) => browser_reply_from_run("cdp", headless, started, run),
        Err(e) => browser_error_reply("cdp", headless, started, e),
    };
    if let Some(obj) = reply.as_object_mut() {
        obj.entry("steps".to_string()).or_insert_with(|| json!([]));
        obj.entry("tab".to_string()).or_insert(Value::Null);
        obj.insert(
            "expect".to_string(),
            if parsed.expect.is_empty() { Value::Null } else { json!({ "ok": true, "report": expect }) },
        );
    }
    browser_lease::release(cwd, &agent);
    reply
}

fn session_file(cwd: &Path, name: &str) -> PathBuf {
    cwd.join(".gm")
        .join("browser-cdp-sessions")
        .join(format!("{name}.json"))
}

// One file per helper run: the helper rewrites it after each completed step, and run_helper_with
// reads and removes it when the helper was killed before printing its final JSON.
fn partial_result_file(cwd: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    cwd.join(".gm")
        .join("browser-cdp-partial")
        .join(format!("{}-{nanos}-{seq}.json", std::process::id()))
}

fn session_target_id(cwd: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(session_file(cwd, name)).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("targetId")?
        .as_str()
        .map(str::to_string)
}

// Tab group colours accepted by session-update (the browser tab-group colour set).
const SESSION_COLORS: [&str; 9] = [
    "grey", "blue", "red", "yellow", "green", "pink", "purple", "cyan", "orange",
];

// Title and colour live in <name>.meta.json, not the helper's session file: the helper rewrites
// that file after every call, which would drop them. Its stem is "<name>.meta", which session_name
// rejects, so session_list never mistakes it for a session.
fn meta_file(cwd: &Path, name: &str) -> PathBuf {
    cwd.join(".gm")
        .join("browser-cdp-sessions")
        .join(format!("{name}.meta.json"))
}

fn read_meta(cwd: &Path, name: &str) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(meta_file(cwd, name))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

fn session_exists(cwd: &Path, name: &str) -> bool {
    session_file(cwd, name).exists() || meta_file(cwd, name).exists()
}

// Same lock as the helper's lockSession (`<session file>.lock`, created with create-new), so an
// edit and a running call on the same session cannot interleave. The lock is removed afterwards.
fn with_session_lock<T>(
    cwd: &Path,
    name: &str,
    body: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let file = session_file(cwd, name);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("session dir: {e}"))?;
    }
    let lock = PathBuf::from(format!("{}.lock", file.display()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .map_err(|e| format!("session '{name}' is in use by another call: {e}"))?;
    let result = body();
    let _ = std::fs::remove_file(&lock);
    result
}

fn write_meta(cwd: &Path, name: &str, meta: &serde_json::Map<String, Value>) -> Result<(), String> {
    let file = meta_file(cwd, name);
    let temp = PathBuf::from(format!("{}.{}.tmp", file.display(), std::process::id()));
    let text = serde_json::to_string_pretty(&Value::Object(meta.clone()))
        .map_err(|e| format!("session metadata: {e}"))?;
    std::fs::write(&temp, text).map_err(|e| format!("session write: {e}"))?;
    std::fs::rename(&temp, &file).map_err(|e| format!("session rename: {e}"))
}

pub fn session_list(cwd: &Path) -> Value {
    let mut names = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(cwd.join(".gm").join("browser-cdp-sessions")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = stem.strip_suffix(".meta").unwrap_or(stem);
            if session_name(name).is_ok() {
                names.insert(name.to_string());
            }
        }
    }
    let sessions: Vec<Value> = names
        .iter()
        .map(|name| {
            let target = std::fs::read_to_string(session_file(cwd, name))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("targetId").cloned())
                .unwrap_or(Value::Null);
            let meta = read_meta(cwd, name);
            json!({
                "name": name,
                "targetId": target,
                "title": meta.get("title").cloned().unwrap_or(Value::Null),
                "color": meta.get("color").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "ok": true, "op": "session-list", "count": sessions.len(), "sessions": sessions })
}

// Deletes the session's state: the helper's file (tab binding, uid map, log cursor) and its
// metadata. Deleting an absent session succeeds with deleted=false.
pub fn session_delete(cwd: &Path, name: &str) -> Value {
    let name = match session_name(name) {
        Ok(name) => name,
        Err(e) => return json!({ "ok": false, "op": "session-delete", "error": e }),
    };
    let result = with_session_lock(cwd, &name, || {
        let existed = session_exists(cwd, &name);
        for file in [session_file(cwd, &name), meta_file(cwd, &name)] {
            if file.exists() {
                std::fs::remove_file(&file).map_err(|e| format!("session delete: {e}"))?;
            }
        }
        Ok(existed)
    });
    match result {
        Ok(deleted) => json!({ "ok": true, "op": "session-delete", "name": name, "deleted": deleted }),
        Err(e) => json!({ "ok": false, "op": "session-delete", "name": name, "error": e }),
    }
}

// Sets the title and/or colour recorded for a session. The colour must be in SESSION_COLORS.
pub fn session_update(cwd: &Path, name: &str, title: Option<&str>, color: Option<&str>) -> Value {
    let fail = |e: String| json!({ "ok": false, "op": "session-update", "error": e });
    let name = match session_name(name) {
        Ok(name) => name,
        Err(e) => return fail(e),
    };
    if title.is_none() && color.is_none() {
        return fail("session-update needs title and/or color".to_string());
    }
    if let Some(c) = color {
        if !SESSION_COLORS.contains(&c) {
            return fail(format!("color must be one of {}, got '{c}'", SESSION_COLORS.join(", ")));
        }
    }
    if let Some(t) = title {
        if t.is_empty() || t.len() > 128 {
            return fail("title must be 1-128 bytes".to_string());
        }
    }
    let result = with_session_lock(cwd, &name, || {
        if !session_exists(cwd, &name) {
            return Err(format!("no session named '{name}'"));
        }
        let mut meta = read_meta(cwd, &name);
        if let Some(t) = title {
            meta.insert("title".to_string(), json!(t));
        }
        if let Some(c) = color {
            meta.insert("color".to_string(), json!(c));
        }
        write_meta(cwd, &name, &meta)?;
        Ok(meta)
    });
    match result {
        Ok(meta) => json!({
            "ok": true,
            "op": "session-update",
            "name": name,
            "title": meta.get("title").cloned().unwrap_or(Value::Null),
            "color": meta.get("color").cloned().unwrap_or(Value::Null),
        }),
        Err(e) => fail(e),
    }
}

// Resets a session's browser connection: removes the helper's session file, so the tab binding,
// the uid map and the log cursor are gone. The title and colour are kept. The next call on this
// session opens or reuses a tab from scratch.
pub fn session_reset(cwd: &Path, name: &str) -> Value {
    let fail = |e: String| json!({ "ok": false, "op": "session-reset", "error": e });
    let name = match session_name(name) {
        Ok(name) => name,
        Err(e) => return fail(e),
    };
    let result = with_session_lock(cwd, &name, || {
        if !session_exists(cwd, &name) {
            return Err(format!("no session named '{name}'"));
        }
        let file = session_file(cwd, &name);
        let existed = file.exists();
        if existed {
            std::fs::remove_file(&file).map_err(|e| format!("session reset: {e}"))?;
        }
        Ok(existed)
    });
    match result {
        Ok(reset) => json!({ "ok": true, "op": "session-reset", "name": name, "reset": reset }),
        Err(e) => fail(e),
    }
}

fn page_targets(port: u16) -> Result<Vec<Value>, String> {
    let text = endpoint_get(port, "/json/list")?;
    let list: Value = serde_json::from_str(&text)
        .map_err(|e| format!("/json/list was not JSON: {e}"))?;
    Ok(list
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("page"))
                .cloned()
                .collect()
        })
        .unwrap_or_default())
}

// Compares the lease's tab state with the expect= line. Returns the state it read, and the
// differences by name when there are any.
fn check_expectations(
    port: u16,
    cwd: &Path,
    session: Option<&str>,
    expect: &Expectation,
) -> Result<Value, String> {
    if expect.is_empty() {
        return Ok(Value::Null);
    }
    let pages = page_targets(port)?;
    let tabs_found = pages.len() as u64;
    let session_tab = session.and_then(|name| session_target_id(cwd, name));
    let session_live = session_tab.as_ref().is_some_and(|id| {
        pages
            .iter()
            .any(|page| page.get("id").and_then(Value::as_str) == Some(id.as_str()))
    });
    let would_open: u64 = if session_live || tabs_found > 0 {
        0
    } else {
        1
    };
    let mut differences: Vec<String> = Vec::new();
    if let Some(expected) = expect.tabs {
        if expected != tabs_found {
            differences.push(format!("tabs: expected {expected}, found {tabs_found}"));
        }
    }
    if let Some(name) = expect.reuse.as_deref() {
        if session != Some(name) {
            differences.push(format!(
                "reuse: expected session {name}, the body names {}",
                session.unwrap_or("no session")
            ));
        } else if !session_live {
            differences.push(format!("reuse: session {name} has no live tab to reuse"));
        }
    }
    if let Some(expected) = expect.open {
        if expected != would_open {
            differences.push(format!(
                "open: expected {expected}, this call would open {would_open}"
            ));
        }
    }
    let report = json!({
        "tabs_found": tabs_found,
        "session_tab": session_tab,
        "session_tab_live": session_live,
        "would_open": would_open,
        "differences": differences,
    });
    if differences.is_empty() {
        Ok(report)
    } else {
        Err(format!("expectation failed: {}", differences.join("; ")))
    }
}

fn cdp_run_config(cwd: &Path, session: Option<&str>, budget: Duration) -> Value {
    let mut config = json!({
        "cwd": cwd.display().to_string(),
        "watchdogMs": (budget.as_millis() as u64).saturating_sub(10_000),
        "partialFile": partial_result_file(cwd).display().to_string(),
    });
    if let Some(name) = session {
        config["session"] = json!({
            "name": name,
            "file": session_file(cwd, name).display().to_string(),
        });
    }
    config
}
