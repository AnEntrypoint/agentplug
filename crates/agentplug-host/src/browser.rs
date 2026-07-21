use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

// Drives a real Chrome via the DevTools protocol instead of shelling out to the
// playwriter CLI, whose relay process crashes with a real, live-reproducible
// Windows libuv assertion on every eval dispatch: "Assertion failed:
// !(handle->flags & UV_HANDLE_CLOSING), file src\win\async.c, line 76"
// (re-confirmed live, twice, on playwriter 0.4.0 -- not a stale/misdiagnosed
// finding; the crash is specific to this Windows async-handle-teardown
// class, not evidence against playwriter's broad cross-platform userbase
// elsewhere). The flow that works on this host, proven end to end: launch
// Chrome headless with --remote-debugging-port, poll its /json/version HTTP
// endpoint for the DevTools websocket, then run the script in-page via
// Runtime.evaluate over that websocket. Chrome exposes CDP over HTTP+WS with
// no external dependency; the one piece needing a websocket client is the
// eval, which a bundled node helper (node has a native WebSocket) performs --
// node is already a required runtime for this environment. No playwriter, no
// relay, no crash.

const CDP_EVAL_JS: &str = include_str!("cdp_eval.js");

/// Live-found (vendor-browser-timing-config-to-gm): every CDP timing
/// constant below was a hardcoded literal, unreachable to a project wanting
/// to tune it (a slower CI runner needing a longer chrome-ready deadline, a
/// project wanting a tighter poll interval, etc) -- agentplug-runner is a
/// native binary, not the wasm guest, so it needs its OWN .gm/-reading
/// mechanism for this (the existing pattern: daemon.rs's
/// instruction_source_config_path reads .gm/instructions/source.json
/// per-project the same way). Read from <project>/.gm/browser-config.json
/// when present; every field is optional and falls back to the exact
/// pre-existing literal, so an unconfigured project behaves byte-identically
/// to before this change.
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
            })
    }
    fn cdp_poll_timeout(&self) -> Duration { Duration::from_millis(self.cdp_poll_timeout_ms.unwrap_or(1000)) }
    fn cdp_poll_interval(&self) -> Duration { Duration::from_millis(self.cdp_poll_interval_ms.unwrap_or(250)) }
    fn chrome_ready_deadline(&self) -> Duration { Duration::from_millis(self.chrome_ready_deadline_ms.unwrap_or(30_000)) }
    fn eval_timeout_grace(&self) -> u64 { self.eval_timeout_grace_ms.unwrap_or(6000) }
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

/// `capture\n<script>` / `profile\n<script>` / `trace\n<script>` mode
/// prefixes, documented in AGENTS.md/SKILL.md's exec_js/browser profiling
/// contract but never implemented in the native agentplug-host browser
/// path (only the retired JS wrapper had them). Stripped before parse_body
/// sees the remaining url=/bare-URL/script body, so the two prefix systems
/// compose (e.g. "profile\nurl=https://...\nscript").
#[derive(Clone, Copy, PartialEq)]
enum BrowserMode {
    Default,
    Capture,
    Profile,
    Trace,
}

fn strip_mode_prefix(body: &str) -> (BrowserMode, &str) {
    let trimmed = body.trim_start();
    for (prefix, mode) in [("capture\n", BrowserMode::Capture), ("profile\n", BrowserMode::Profile), ("trace\n", BrowserMode::Trace)] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (mode, rest);
        }
    }
    (BrowserMode::Default, body)
}

fn browser_profiles_dir(cwd: &Path) -> PathBuf {
    cwd.join(".gm").join("browser-profiles")
}

/// Live-found (browser-profile-not-persisted-locally-to-project): Chrome's
/// --user-data-dir previously lived under std::env::temp_dir() and was
/// std::fs::remove_dir_all'd at the end of EVERY dispatch -- zero session
/// persistence (cookies, localStorage, login state, cache) across
/// dispatches, and the profile lived outside the project entirely, contrary
/// to the "locally profiled to the project" requirement. Fixed: the Chrome
/// user-data-dir now lives under <project>/.gm/browser-chrome-profile-
/// <session_id>/ (distinct from browser_profiles_dir above, which holds
/// CPU/trace ARTIFACT files from the profile/trace modes, not the live
/// Chrome profile) and is NEVER deleted after a dispatch -- only a fresh
/// dispatch under the SAME session_id reuses it, so state genuinely
/// persists across a debugging session. `sanitize` keeps the directory name
/// filesystem-safe the same way the old temp-dir stamp already did.
fn browser_chrome_profile_dir(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(".gm").join(format!("browser-chrome-profile-{}", sanitize(session_id)))
}

pub fn run(body: &str, cwd: &Path, session_id: &str) -> Value {
    let Some(chrome) = find_chrome() else {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "no Chrome found; install Google Chrome or Chromium"});
    };
    let Some(node) = which("node") else {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "node not found on PATH; required to drive Chrome over CDP"});
    };

    let t0 = Instant::now();
    let browser_cfg = BrowserConfig::load(cwd);
    // The guest hands the raw spool dispatch JSON ({"body": "...", "timeoutMs": N})
    // as `body`, not the browser script directly -- extract the actual script and
    // timeout from that envelope before parsing prefixes. A bare string body (no
    // JSON envelope) is used as-is.
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
    let (mode, after_mode) = strip_mode_prefix(&inner_body);
    let (start_url, script) = parse_body(after_mode);

    let tmp = std::env::temp_dir();
    let stamp = format!("{}-{}", std::process::id(), sanitize(session_id));
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let _ = std::fs::create_dir_all(&profile_dir);
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    let artifact_path = if mode != BrowserMode::Default {
        let dir = browser_profiles_dir(cwd);
        let _ = std::fs::create_dir_all(&dir);
        let ext = match mode { BrowserMode::Trace => "trace.json", _ => "profile.json" };
        Some(dir.join(format!("{}-{}.{}", mode_label(mode), unix_ms(), ext)))
    } else {
        None
    };
    if let Ok(mut f) = std::fs::File::create(&helper_path) {
        let _ = f.write_all(CDP_EVAL_JS.as_bytes());
    }
    if let Ok(mut f) = std::fs::File::create(&script_path) {
        let _ = f.write_all(script.as_bytes());
    }

    let port = free_port();
    let mut chrome_child = match Command::new(&chrome)
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-default-apps")
        .arg("--disable-gpu")
        .arg("--headless=new")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            cleanup(&[&helper_path, &script_path, &result_path]);
            return json!({"ok": false, "stdout": "", "exit_code": 1,
                "stderr": format!("chrome launch failed: {e}")});
        }
    };

    if !cdp_ready(port, Instant::now() + browser_cfg.chrome_ready_deadline(), &browser_cfg) {
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();
        cleanup(&[&helper_path, &script_path, &result_path]);
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": format!("chrome CDP endpoint did not become ready within {}ms", browser_cfg.chrome_ready_deadline().as_millis())});
    }

    let cfg = json!({
        "port": port,
        "startUrl": start_url,
        "scriptFile": script_path.to_string_lossy(),
        "resultFile": result_path.to_string_lossy(),
        "timeoutMs": timeout_ms,
        "mode": mode_label(mode),
        "artifactFile": artifact_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    })
    .to_string();

    let spawn = Command::new(&node)
        .arg(&helper_path)
        .arg(&cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            let _ = chrome_child.kill();
            let _ = chrome_child.wait();
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

    let mut stderr_buf = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_end(&mut err, &mut stderr_buf);
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);

    let result_value: Value = std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);

    let _ = chrome_child.kill();
    let _ = chrome_child.wait();
    cleanup(&[&helper_path, &script_path, &result_path]);
    // profile_dir is now the persistent per-session Chrome profile under
    // .gm/browser-chrome-profile-<session_id>/ -- deliberately NOT removed
    // here (see browser_chrome_profile_dir's doc comment); it survives so
    // the next dispatch under the same session_id reuses cookies/
    // localStorage/cache instead of starting cold every time.

    let cdp_error = result_value.get("__cdpError").and_then(|v| v.as_str());
    let ok = exit_code == 0 && !timed_out && cdp_error.is_none();
    let mut out = json!({
        "ok": ok,
        "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": t0.elapsed().as_millis() as u64,
    });
    if cdp_error.is_some() {
        out["result"] = Value::Null;
        return out;
    }
    // Non-default modes get a {result, <mode-key>} envelope from
    // cdp_eval.js (mirrors exec_js's opts.mem/opts.profile envelope shape);
    // default mode is the bare returned value, unchanged.
    match mode {
        BrowserMode::Default => {
            out["result"] = result_value;
        }
        BrowserMode::Capture => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["debug"] = result_value.get("debug").cloned().unwrap_or(json!({"console": [], "network": [], "performance": null}));
        }
        BrowserMode::Profile => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["profile"] = result_value.get("profile").cloned().unwrap_or(json!({"timeframe": null, "culprits": []}));
            if let Some(p) = &artifact_path {
                out["profile_file"] = json!(p.to_string_lossy());
            }
        }
        BrowserMode::Trace => {
            out["result"] = result_value.get("result").cloned().unwrap_or(Value::Null);
            out["trace"] = result_value.get("trace").cloned().unwrap_or(json!({"wall_us": 0, "gpu_us": 0, "viz_us": 0, "cc_us": 0, "by_category": {}}));
            if let Some(p) = &artifact_path {
                out["trace_file"] = json!(p.to_string_lossy());
            }
        }
    }
    out
}

fn mode_label(mode: BrowserMode) -> &'static str {
    match mode {
        BrowserMode::Default => "default",
        BrowserMode::Capture => "capture",
        BrowserMode::Profile => "profile",
        BrowserMode::Trace => "trace",
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
