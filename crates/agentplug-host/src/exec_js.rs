use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

const RESULT_SENTINEL: &str = "__GM_RESULT__";

/// Ported from rs-plugkit's gm-runner/src/exec_js.rs -- same subprocess
/// dispatch + sentinel-based return-value capture, so agentplug-host's
/// host_exec_js has real backing instead of the not_implemented stub every
/// other still-missing host import uses.
pub fn run(code: &str, opts: &Value, cwd: &Path) -> Value {
    let lang = opts.get("lang").and_then(|v| v.as_str()).unwrap_or("nodejs");
    let timeout_ms = match opts.get("timeoutMs").and_then(|v| v.as_i64()) {
        Some(ms) if ms >= 100 => ms as u64,
        Some(ms) => {
            return json!({
                "ok": false, "error": "timeoutMs below floor", "min": 100, "received": ms,
            });
        }
        None => {
            return json!({
                "ok": false, "error": "missing timeoutMs",
                "required": "positive integer milliseconds",
            });
        }
    };

    let is_js_lang = lang == "nodejs" || lang == "js";
    let (cmd, args, script_file) = match build_command(lang, code) {
        Some(v) => v,
        None => return json!({"ok": false, "error": format!("unsupported lang: {lang}")}),
    };

    let t0 = Instant::now();
    let mut command = Command::new(&cmd);
    command
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let spawn = command.spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false, "stdout": "", "stderr": e.to_string(), "exit_code": -1,
                "spawn_error": {"message": e.to_string()},
            });
        }
    };

    let timed_out = match child.wait_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Some(_status)) => false,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        Err(_) => false,
    };

    let duration_ms = t0.elapsed().as_millis() as u64;
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_end(&mut out, &mut stdout_buf);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_end(&mut err, &mut stderr_buf);
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);

    if let Some(f) = script_file {
        let _ = std::fs::remove_file(f);
    }

    let mut stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let mut result_field: Option<Value> = None;

    if is_js_lang {
        if let Some(idx) = stdout.rfind(RESULT_SENTINEL) {
            let tail = &stdout[idx + RESULT_SENTINEL.len()..];
            let line_end = tail.find('\n').unwrap_or(tail.len());
            let json_str = &tail[..line_end];
            if let Ok(parsed) = serde_json::from_str::<Value>(json_str) {
                result_field = Some(parsed);
            }
            let mut cleaned = String::new();
            cleaned.push_str(&stdout[..idx]);
            if let Some(rest_start) = tail.get(line_end + 1..) {
                cleaned.push_str(rest_start);
            }
            if cleaned.ends_with('\n') {
                cleaned.pop();
            }
            stdout = cleaned;
        }
    }

    let mut v = json!({
        "ok": exit_code == 0,
        "stdout": stdout,
        "stderr": String::from_utf8_lossy(&stderr_buf),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": duration_ms,
    });
    if let Some(r) = result_field {
        v["result"] = r;
    }
    v
}

fn build_command(lang: &str, code: &str) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    match lang {
        "nodejs" | "js" => {
            let wrapped = format!(
                "(async () => {{\n  try {{\n    const __r = await (async () => {{\n{code}\n}})();\n    try {{ console.log('{RESULT_SENTINEL}' + JSON.stringify(__r === undefined ? null : __r)); }}\n    catch (__se) {{ console.log('{RESULT_SENTINEL}' + JSON.stringify({{ __unserializable: String(__se && __se.message || __se) }})); }}\n  }} catch (__e) {{\n    console.error(String(__e && __e.stack || __e));\n    process.exitCode = 1;\n  }}\n}})();\n"
            );
            Some((resolve_node_cmd(), vec!["-e".to_string(), wrapped], None))
        }
        "python" | "py" => Some(("python".to_string(), vec!["-c".to_string(), code.to_string()], None)),
        "bash" | "sh" | "shell" => Some((resolve_bash_cmd(), vec!["-c".to_string(), code.to_string()], None)),
        "powershell" | "ps1" => Some((
            "powershell".to_string(),
            vec!["-NoProfile".to_string(), "-NonInteractive".to_string(), "-Command".to_string(), code.to_string()],
            None,
        )),
        "deno" => Some(("deno".to_string(), vec!["eval".to_string(), code.to_string()], None)),
        _ => None,
    }
}

fn resolve_node_cmd() -> String {
    for candidate in ["node", "bun"] {
        if let Some(p) = which(candidate) {
            return p.to_string_lossy().into_owned();
        }
    }
    "node".to_string()
}

// System32\bash.exe on Windows is the WSL launcher stub, not a real POSIX
// shell -- it either hangs waiting on a WSL distro or behaves unlike Git
// Bash. Prefer Git Bash's real bash.exe explicitly, falling back to `which`
// only if the well-known Git-for-Windows path isn't present.
fn resolve_bash_cmd() -> String {
    if cfg!(windows) {
        let git_bash = std::path::Path::new("C:\\Program Files\\Git\\bin\\bash.exe");
        if git_bash.exists() {
            return git_bash.to_string_lossy().into_owned();
        }
        let git_bash_usr = std::path::Path::new("C:\\Program Files\\Git\\usr\\bin\\bash.exe");
        if git_bash_usr.exists() {
            return git_bash_usr.to_string_lossy().into_owned();
        }
    }
    which("bash").map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "bash".to_string())
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_name = if cfg!(windows) { format!("{cmd}.exe") } else { cmd.to_string() };
    std::env::split_paths(&path_var).map(|p| p.join(&exe_name)).find(|p| p.exists())
}

#[allow(dead_code)]
fn write_script(prefix: &str, content: &str) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}.js", std::process::id()));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(path)
}
