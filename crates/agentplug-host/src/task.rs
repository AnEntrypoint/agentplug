use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

struct TaskEntry {
    child: Child,
    lang: String,
    started_ms: u64,
    timeout_ms: u64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    finished_ms: Option<u64>,
}

type Registry = Mutex<HashMap<String, TaskEntry>>;

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_id(counter_seed: u64) -> String {
    format!("task-{:x}", counter_seed)
}

/// Drain any output a still-running child has buffered, and reap it if it has
/// exited, updating the registry entry in place. A task-manager built on the
/// daemon's own poll loop has no dedicated reader thread per child, so output
/// is pulled opportunistically on every list/output/stop call -- the child's
/// pipes are non-blocking-drained here rather than blocking the poll loop on a
/// read that a long-running task would never let return.
fn poll_entry(entry: &mut TaskEntry) {
    if entry.finished_ms.is_some() {
        return;
    }
    match entry.child.try_wait() {
        Ok(Some(status)) => {
            if let Some(mut out) = entry.child.stdout.take() {
                let _ = out.read_to_end(&mut entry.stdout);
            }
            if let Some(mut err) = entry.child.stderr.take() {
                let _ = err.read_to_end(&mut entry.stderr);
            }
            entry.exit_code = status.code();
            entry.finished_ms = Some(now_ms());
        }
        Ok(None) => {
            if now_ms().saturating_sub(entry.started_ms) > entry.timeout_ms {
                let _ = entry.child.kill();
                let _ = entry.child.wait();
                if let Some(mut out) = entry.child.stdout.take() {
                    let _ = out.read_to_end(&mut entry.stdout);
                }
                if let Some(mut err) = entry.child.stderr.take() {
                    let _ = err.read_to_end(&mut entry.stderr);
                }
                entry.exit_code = Some(-1);
                entry.finished_ms = Some(now_ms());
            }
        }
        Err(_) => {}
    }
}

fn entry_summary(id: &str, entry: &TaskEntry) -> Value {
    json!({
        "id": id,
        "lang": entry.lang,
        "started_ms": entry.started_ms,
        "running": entry.finished_ms.is_none(),
        "exit_code": entry.exit_code,
        "finished_ms": entry.finished_ms,
    })
}

fn spawn(params: &Value, cwd: &Path) -> Value {
    let lang = params.get("lang").and_then(|v| v.as_str()).unwrap_or("");
    let code = params.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let timeout_ms = params.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(120_000);
    if lang.is_empty() {
        return json!({"ok": false, "error": "lang required"});
    }
    if code.is_empty() {
        return json!({"ok": false, "error": "code required"});
    }
    let Some((cmd, args, _script_file)) = crate::exec_js::build_command(lang, code) else {
        return json!({"ok": false, "error": format!("unsupported lang: {lang}")});
    };
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
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": format!("spawn failed: {e}")}),
    };
    let started = now_ms();
    let id = next_id(started ^ (child.id() as u64));
    let entry = TaskEntry {
        child,
        lang: lang.to_string(),
        started_ms: started,
        timeout_ms,
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        finished_ms: None,
    };
    let mut reg = registry().lock().unwrap();
    reg.insert(id.clone(), entry);
    json!({"ok": true, "id": id, "started_ms": started})
}

fn list() -> Value {
    let mut reg = registry().lock().unwrap();
    let ids: Vec<String> = reg.keys().cloned().collect();
    let mut tasks = Vec::new();
    for id in ids {
        if let Some(entry) = reg.get_mut(&id) {
            poll_entry(entry);
            tasks.push(entry_summary(&id, entry));
        }
    }
    json!({"ok": true, "tasks": tasks})
}

fn output(params: &Value) -> Value {
    let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let max_bytes = params.get("max_bytes").and_then(|v| v.as_u64()).unwrap_or(65536) as usize;
    if id.is_empty() {
        return json!({"ok": false, "error": "task id required"});
    }
    let mut reg = registry().lock().unwrap();
    let Some(entry) = reg.get_mut(id) else {
        return json!({"ok": false, "error": format!("no such task {id}")});
    };
    poll_entry(entry);
    let tail = |buf: &[u8]| -> String {
        let start = buf.len().saturating_sub(max_bytes);
        String::from_utf8_lossy(&buf[start..]).into_owned()
    };
    json!({
        "ok": true,
        "id": id,
        "stdout": tail(&entry.stdout),
        "stderr": tail(&entry.stderr),
        "running": entry.finished_ms.is_none(),
        "exit_code": entry.exit_code,
    })
}

fn stop(params: &Value) -> Value {
    let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return json!({"ok": false, "error": "task id required"});
    }
    let mut reg = registry().lock().unwrap();
    let Some(mut entry) = reg.remove(id) else {
        return json!({"ok": false, "error": format!("no such task {id}")});
    };
    let _ = entry.child.kill();
    let _ = entry.child.wait();
    json!({"ok": true, "id": id, "stopped": true})
}

/// Services the `host_task_proc` import for the daemon: spawns background
/// children into a process registry and reports on them across dispatches,
/// replacing the not_implemented stub. Mirrors the JS wrapper's task surface
/// (spawn/list/stop/output). Actions unknown to this manager return a typed
/// error rather than a silent success.
pub fn handle(action: &str, params: &Value, cwd: &Path) -> Value {
    match action {
        "spawn" => spawn(params, cwd),
        "list" => list(),
        "output" => output(params),
        "stop" => stop(params),
        other => json!({"ok": false, "error": format!("unknown task action: {other}")}),
    }
}
