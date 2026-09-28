use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use crate::credentials::{
    process_env_carries_github_token, stderr_is_git_auth_rejection, GITHUB_TOKEN_ENV_KEYS_STRIPPED_FOR_CREDENTIAL_HELPER_RESOLUTION,
    GIT_CREDENTIAL_REJECTED_ERROR_CODE, GIT_CREDENTIAL_REJECTED_HINT,
};

const GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE: u64 = 300_000;
const HOST_GIT_STDOUT_CAP_BYTES: usize = 1_048_576;
const HOST_GIT_STDERR_CAP_BYTES: usize = 262_144;
const STALE_ENV_TOKEN_RETRY_NOTE: &str =
    "the runner environment's GITHUB_TOKEN/GH_TOKEN was rejected; retried once with those variables removed so the git credential helper resolved a fresh token";

pub fn git_subprocess_timeout_ms() -> u64 {
    std::env::var("AGENTPLUG_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE)
}

fn drain_reader_capped<R: std::io::Read + Send + 'static>(
    mut reader: R,
    cap: usize,
    cap_hit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(buf.len());
                    if room == 0 {
                        truncated = true;
                        cap_hit.store(true, Ordering::SeqCst);
                        break;
                    }
                    let take = room.min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                        cap_hit.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        (buf, truncated)
    })
}

fn run_git_once(argv: &[String], cwd: &Path, strip_github_token_env: bool) -> serde_json::Value {
    let mut git_cmd = std::process::Command::new("git");
    git_cmd.args(argv).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if strip_github_token_env {
        for key in GITHUB_TOKEN_ENV_KEYS_STRIPPED_FOR_CREDENTIAL_HELPER_RESOLUTION {
            git_cmd.env_remove(key);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        git_cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match git_cmd.spawn() {
        Ok(mut child) => {
            let cap_hit = Arc::new(AtomicBool::new(false));
            let out_handle = child.stdout.take().map(|o| drain_reader_capped(o, HOST_GIT_STDOUT_CAP_BYTES, cap_hit.clone()));
            let err_handle = child.stderr.take().map(|e| drain_reader_capped(e, HOST_GIT_STDERR_CAP_BYTES, cap_hit.clone()));
            let timeout_ms = git_subprocess_timeout_ms();
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut exit_code: i32 = -1;
            let mut timed_out = false;
            let mut cap_stopped = false;
            loop {
                if cap_hit.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    cap_stopped = true;
                    break;
                }
                match child.wait_timeout(Duration::from_millis(20)) {
                    Ok(Some(status)) => {
                        exit_code = status.code().unwrap_or(-1);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
            }
            let (stdout, out_trunc_read) = out_handle.and_then(|h| h.join().ok()).unwrap_or_default();
            let (stderr, _err_trunc) = err_handle.and_then(|h| h.join().ok()).unwrap_or_default();
            if timed_out {
                serde_json::json!({
                    "stdout": String::from_utf8_lossy(&stdout),
                    "stderr": format!("git {argv:?} timed out after {timeout_ms}ms, killed"),
                    "exit_code": -1,
                })
            } else {
                serde_json::json!({
                    "stdout": String::from_utf8_lossy(&stdout),
                    "stderr": String::from_utf8_lossy(&stderr),
                    "exit_code": if cap_stopped { 0 } else { exit_code },
                    "stdout_truncated": out_trunc_read || cap_stopped,
                })
            }
        }
        Err(e) => serde_json::json!({"stdout": "", "stderr": e.to_string(), "exit_code": 1}),
    }
}

fn result_is_auth_rejection(result: &serde_json::Value) -> bool {
    result.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) != 0
        && result.get("stderr").and_then(|s| s.as_str()).map(stderr_is_git_auth_rejection).unwrap_or(false)
}

fn annotate(result: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(field.to_string(), value);
    }
}

fn annotate_credential_rejection(result: &mut serde_json::Value) {
    let stderr = result.get("stderr").and_then(|s| s.as_str()).unwrap_or("").trim_end().to_string();
    annotate(result, "stderr", serde_json::Value::String(format!("{stderr}\n{GIT_CREDENTIAL_REJECTED_HINT}")));
    annotate(result, "error_code", serde_json::Value::String(GIT_CREDENTIAL_REJECTED_ERROR_CODE.to_string()));
}

pub fn run_git_with_credential_recovery(argv: &[String], cwd: &Path) -> serde_json::Value {
    let mut result = run_git_once(argv, cwd, false);
    if !result_is_auth_rejection(&result) {
        return result;
    }
    if process_env_carries_github_token() {
        result = run_git_once(argv, cwd, true);
        if result.get("exit_code").and_then(|c| c.as_i64()) == Some(0) {
            annotate(&mut result, "credential_env_retry", serde_json::Value::String(STALE_ENV_TOKEN_RETRY_NOTE.to_string()));
            return result;
        }
    }
    if result_is_auth_rejection(&result) {
        annotate_credential_rejection(&mut result);
    }
    result
}
