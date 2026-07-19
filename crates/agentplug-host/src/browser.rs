use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use wait_timeout::ChildExt;

/// Shells out to the same `playwriter` CLI plugkit-wasm-wrapper.js's
/// host_browser_exec drives (BROWSER_RUNNER_BIN, resolved via `bun x` /
/// `npx -y` / a global install) rather than adopting an in-process Rust CDP
/// crate (chromiumoxide/headless_chrome) -- the JS wrapper's browser verb
/// carries substantial session-lifecycle state (per-session PLAYWRITER_HOME
/// socket dirs, port tracking files, idle-session reaping) that a faithful
/// native port needs its own dedicated pass to replicate correctly; this
/// covers the core single-shot dispatch shape (resolve runner, spawn with
/// the right env/timeout, pass stdout/stderr through) so `browser` verb
/// dispatches work end to end today. Full session-lifecycle parity
/// (idle reaping, orphan cleanup, multi-session port tracking) is tracked
/// as its own follow-up, not silently dropped.
pub fn run(body: &str, cwd: &Path, session_id: &str) -> Value {
    let Some((cmd, base_args)) = find_browser_runner() else {
        return json!({
            "ok": false, "stdout": "", "exit_code": 1,
            "stderr": "managed browser session runner 'playwriter' not found on PATH or in npm-global; install with 'bun add -g playwriter' or 'npm i -g playwriter'",
        });
    };

    let timeout_ms: u64 = 120_000;
    let sock_dir = cwd.join(".gm").join("playwriter-home").join(sanitize(session_id));
    let _ = std::fs::create_dir_all(&sock_dir);

    let trimmed = body.trim();
    let mut args = base_args;
    // Matches plugkit-wasm-wrapper.js's argv shapes exactly: `session ...`
    // dispatches split into discrete argv entries (['session','new'], never
    // a single "session new" string -- playwriter's own CLI parser expects
    // that), everything else is an eval body passed via -s/-e.
    if trimmed == "session new" || trimmed.is_empty() {
        args.push("session".to_string());
        args.push("new".to_string());
    } else if let Some(rest) = trimmed.strip_prefix("session ") {
        args.push("session".to_string());
        args.extend(rest.split_whitespace().map(String::from));
    } else {
        args.push("-s".to_string());
        args.push(session_id.to_string());
        args.push("--timeout".to_string());
        args.push(timeout_ms.to_string());
        args.push("-e".to_string());
        args.push(body.to_string());
    }

    let t0 = std::time::Instant::now();
    let spawn = Command::new(&cmd)
        .args(&args)
        .current_dir(cwd)
        .env("PLAYWRITER_HOME", &sock_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            return json!({"ok": false, "stdout": "", "stderr": e.to_string(), "exit_code": 1});
        }
    };

    let timed_out = match child.wait_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Some(_)) => false,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        Err(_) => false,
    };

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_end(&mut out, &mut stdout_buf);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_end(&mut err, &mut stderr_buf);
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);

    json!({
        "ok": exit_code == 0 && !timed_out,
        "stdout": scrub(&String::from_utf8_lossy(&stdout_buf)),
        "stderr": scrub(&String::from_utf8_lossy(&stderr_buf)),
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

fn scrub(s: &str) -> String {
    // Matches plugkit-wasm-wrapper.js scrubBrowserRunnerText's product-name
    // masking so responses look identical regardless of which host served
    // them.
    s.replace("playwriter", "managed browser session")
        .replace("playwright", "managed browser session")
        .replace("puppeteer", "managed browser session")
}

/// Mirrors plugkit-wasm-wrapper.js's findBrowserRunner precedence exactly --
/// the prior version of this function tried a raw `which("playwriter")`
/// FIRST, which on a system where playwriter was only ever installed via
/// `bun x` resolves to a bare content-addressed cache copy
/// (`~/.bun/install/cache/playwriter@version`) that has no node_modules of
/// its own, so invoking its bin.js directly fails dependency resolution
/// (live-witnessed: "Cannot find package 'goke' from .../playwriter@0.4.0.../dist/cli.js").
/// `bun x playwriter@latest` sets up real dependency resolution for the same
/// cached copy and does not fail. The JS wrapper already fixed this
/// (patchAllCachedPlaywriterCopies + bun-x-before-raw-which ordering); this
/// port applies the same fix so gm-runner's browser dispatch does not
/// regress into the bug the JS wrapper already solved.
fn find_browser_runner() -> Option<(String, Vec<String>)> {
    let bun_global = directories::BaseDirs::new().map(|b| {
        b.home_dir()
            .join(".bun")
            .join("install")
            .join("global")
            .join("node_modules")
            .join("playwriter")
            .join("bin.js")
    });
    if let Some(p) = &bun_global {
        if p.exists() {
            // bin.js is a JS entrypoint -- invoke it with a real JS runtime
            // on PATH (node preferred, bun as fallback), never gm-runner's
            // own (non-JS) executable.
            let runtime = which("node").or_else(|| which("bun"));
            if let Some(rt) = runtime {
                return Some((rt.to_string_lossy().into_owned(), vec![p.to_string_lossy().into_owned()]));
            }
        }
    }
    if which("bun").is_some() {
        return Some(("bun".to_string(), vec!["x".to_string(), "playwriter@latest".to_string()]));
    }
    if which("npx").is_some() {
        return Some(("npx".to_string(), vec!["-y".to_string(), "playwriter".to_string()]));
    }
    // Raw `which("playwriter")` is the LAST resort, not the first -- it is
    // the exact broken-cached-bin.js path when it resolves to a bun-cache
    // copy rather than a real npm-global install.
    if let Some(p) = which("playwriter") {
        return Some((p.to_string_lossy().into_owned(), vec![]));
    }
    None
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_name = if cfg!(windows) { format!("{cmd}.exe") } else { cmd.to_string() };
    let cmd_name = if cfg!(windows) { format!("{cmd}.cmd") } else { cmd.to_string() };
    std::env::split_paths(&path_var)
        .find_map(|p| {
            let e = p.join(&exe_name);
            if e.exists() {
                return Some(e);
            }
            let c = p.join(&cmd_name);
            if c.exists() {
                return Some(c);
            }
            None
        })
}
