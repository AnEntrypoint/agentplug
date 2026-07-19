use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

// Drives a real Chrome via the DevTools protocol instead of shelling out to the
// playwriter CLI, whose relay process crashes with a UV_HANDLE_CLOSING native
// assertion (STATUS_STACK_BUFFER_OVERRUN) on Windows on every session dispatch.
// The flow that works on this host, proven end to end: launch Chrome headless
// with --remote-debugging-port, poll its /json/version HTTP endpoint for the
// DevTools websocket, then run the script in-page via Runtime.evaluate over
// that websocket. Chrome exposes CDP over HTTP+WS with no external dependency;
// the one piece needing a websocket client is the eval, which a bundled node
// helper (node has a native WebSocket) performs -- node is already a required
// runtime for this environment. No playwriter, no relay, no crash.

const CDP_EVAL_JS: &str = include_str!("cdp_eval.js");

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

fn cdp_ready(port: u16, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        let url = format!("http://127.0.0.1:{port}/json/version");
        if let Ok(resp) = ureq::get(&url).timeout(Duration::from_millis(1000)).call() {
            if let Ok(body) = resp.into_string() {
                if body.contains("webSocketDebuggerUrl") {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
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
    let (start_url, script) = parse_body(&inner_body);

    let tmp = std::env::temp_dir();
    let stamp = format!("{}-{}", std::process::id(), sanitize(session_id));
    let profile_dir = tmp.join(format!("agentplug-chrome-{stamp}"));
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    let _ = std::fs::create_dir_all(&profile_dir);
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

    if !cdp_ready(port, Instant::now() + Duration::from_millis(30_000)) {
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();
        cleanup(&[&helper_path, &script_path, &result_path]);
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "chrome CDP endpoint did not become ready within 30s"});
    }

    let cfg = json!({
        "port": port,
        "startUrl": start_url,
        "scriptFile": script_path.to_string_lossy(),
        "resultFile": result_path.to_string_lossy(),
        "timeoutMs": timeout_ms,
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

    let timed_out = match child.wait_timeout(Duration::from_millis(timeout_ms + 6000)) {
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
    let _ = std::fs::remove_dir_all(&profile_dir);

    let cdp_error = result_value.get("__cdpError").and_then(|v| v.as_str());
    let ok = exit_code == 0 && !timed_out && cdp_error.is_none();
    json!({
        "ok": ok,
        "result": if cdp_error.is_some() { Value::Null } else { result_value.clone() },
        "stderr": String::from_utf8_lossy(&stderr_buf).into_owned(),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": t0.elapsed().as_millis() as u64,
    })
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
