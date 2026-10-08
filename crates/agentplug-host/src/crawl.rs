use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::http_agent::shared_agent;

const CRAWL_HELPER_JS: &str = include_str!("crawl_cdp.mjs");
const CDP_READY_DEADLINE: Duration = Duration::from_secs(30);
const CDP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HELPER_BUDGET: Duration = Duration::from_secs(300);
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
        let _ = self.0.kill();
        let _ = self.0.wait();
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
        steps.push(parse_step(line).map_err(|e| format!("line {number}: {e}"))?);
    }
    finish(engine, steps)
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

/// Polls `/json/version` on the local CDP port with a fixed interval until it
/// answers with a websocket debugger URL or the deadline passes.
pub fn endpoint_ready(port: u16, deadline: Instant, interval: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/json/version");
    loop {
        if let Ok(resp) = shared_agent()
            .get(&url)
            .timeout(Duration::from_millis(800))
            .call()
        {
            if resp
                .into_string()
                .map(|body| body.contains("webSocketDebuggerUrl"))
                .unwrap_or(false)
            {
                return true;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(interval.min(deadline - now));
    }
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GM_BROWSER_CHROME_PATH")
        .or_else(|| std::env::var_os("CHROME_PATH"))
        .map(PathBuf::from)
        .filter(|p| p.exists())
    {
        return Some(p);
    }
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/bin/google-chrome"),
            PathBuf::from("/usr/bin/chromium"),
            PathBuf::from("/usr/bin/chromium-browser"),
        ]
    };
    candidates
        .into_iter()
        .find(|c| c.exists())
        .or_else(|| find_on_path("chrome"))
        .or_else(|| find_on_path("google-chrome"))
        .or_else(|| find_on_path("chromium"))
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
) -> Result<HelperRun, String> {
    let node = find_on_path("node").ok_or("node is required on PATH to run the crawl helper")?;
    let config = json!({
        "port": port,
        "targetId": target_id,
        "steps": steps,
        "pageTimeoutMs": 30000,
        "textLimit": 20000,
    });
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

/// Headful Chrome over CDP: one Chrome per call, killed when the call returns.
pub fn crawl_cdp(cwd: &Path, body: &str) -> Value {
    let started = Instant::now();
    let parsed = match parse_crawl_body(body) {
        Ok(parsed) => parsed,
        Err(e) => return crawl_error_reply("cdp", false, started, e),
    };
    if parsed.engine.as_deref() == Some("lightpanda") {
        return crawl_error_reply(
            "cdp",
            false,
            started,
            "engine=lightpanda is served by the agentplug-lightpanda plugin, not crawl_cdp"
                .to_string(),
        );
    }
    let Some(chrome) = find_chrome() else {
        return crawl_error_reply(
            "cdp",
            false,
            started,
            "no Chrome found: set GM_BROWSER_CHROME_PATH or CHROME_PATH, or install Google Chrome or Chromium"
                .to_string(),
        );
    };
    let port = match free_local_port() {
        Ok(port) => port,
        Err(e) => return crawl_error_reply("cdp", false, started, e),
    };
    let profile = cwd.join(".gm").join("crawl-cdp-profile");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(e) = std::fs::create_dir_all(&profile) {
        return crawl_error_reply(
            "cdp",
            false,
            started,
            format!("could not create Chrome profile {}: {e}", profile.display()),
        );
    }
    let mut cmd = Command::new(&chrome);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-default-apps",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::windowless::apply_windowless(&mut cmd);
    let _chrome = match cmd.spawn() {
        Ok(child) => KillOnDrop(child),
        Err(e) => {
            return crawl_error_reply(
                "cdp",
                false,
                started,
                format!("Chrome launch failed ({}): {e}", chrome.display()),
            )
        }
    };
    if !endpoint_ready(port, Instant::now() + CDP_READY_DEADLINE, CDP_POLL_INTERVAL) {
        return crawl_error_reply(
            "cdp",
            false,
            started,
            format!(
                "Chrome CDP endpoint on port {port} did not become ready within {}ms (Chrome must be able to open a window: check DISPLAY on Linux)",
                CDP_READY_DEADLINE.as_millis()
            ),
        );
    }
    match run_helper(port, None, &parsed.steps, HELPER_BUDGET) {
        Ok(run) => crawl_reply_from_run("cdp", false, started, run),
        Err(e) => crawl_error_reply("cdp", false, started, e),
    }
}
