use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct TaskEntry {
    child: Child,
    containment: crate::process_tree::ProcessContainment,
    lang: String,
    started_ms: u64,
    deadline: Instant,
    stdout_pipe: Option<DrainedPipe>,
    stderr_pipe: Option<DrainedPipe>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    finished_ms: Option<u64>,
}

pub(crate) struct DrainedPipe {
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

pub(crate) fn drain_in_background(mut pipe: impl Read + Send + 'static) -> DrainedPipe {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 16384];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]),
            }
        }
    });
    DrainedPipe { buffer, reader }
}

impl DrainedPipe {
    pub(crate) fn is_finished(&self) -> bool {
        self.reader.is_finished()
    }

    pub(crate) fn collect(self) -> Vec<u8> {
        let _ = self.reader.join();
        self.buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn collect_with_grace(self, deadline: Instant) -> Vec<u8> {
        while !self.reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

const ADOPTED_TASK_TIMEOUT_MS: u64 = 30 * 60 * 1000;

fn deadline_after(started: Instant, timeout_ms: u64) -> Option<Instant> {
    started.checked_add(Duration::from_millis(timeout_ms))
}

fn harvest_finished_drains(entry: &mut TaskEntry) {
    if entry
        .stdout_pipe
        .as_ref()
        .is_some_and(DrainedPipe::is_finished)
    {
        entry.stdout = entry
            .stdout_pipe
            .take()
            .expect("checked stdout pipe")
            .collect();
    }
    if entry
        .stderr_pipe
        .as_ref()
        .is_some_and(DrainedPipe::is_finished)
    {
        entry.stderr = entry
            .stderr_pipe
            .take()
            .expect("checked stderr pipe")
            .collect();
    }
}

fn drains_pending(entry: &TaskEntry) -> bool {
    entry.stdout_pipe.is_some() || entry.stderr_pipe.is_some()
}

type Registry = Mutex<HashMap<String, TaskEntry>>;

fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ensure_reaper_running() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
            for entry in reg.values_mut() {
                poll_entry(entry);
            }
        });
    });
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

fn poll_entry(entry: &mut TaskEntry) {
    if entry.finished_ms.is_some() {
        if drains_pending(entry) && Instant::now() >= entry.deadline {
            crate::process_tree::terminate_containment(&entry.containment, entry.child.id());
        }
        harvest_finished_drains(entry);
        return;
    }
    match entry.child.try_wait() {
        Ok(Some(status)) => {
            if entry.stdout_pipe.is_none() {
                if let Some(mut out) = entry.child.stdout.take() {
                    let _ = out.read_to_end(&mut entry.stdout);
                }
            }
            if entry.stderr_pipe.is_none() {
                if let Some(mut err) = entry.child.stderr.take() {
                    let _ = err.read_to_end(&mut entry.stderr);
                }
            }
            entry.exit_code = status.code();
            entry.finished_ms = Some(now_ms());
            harvest_finished_drains(entry);
        }
        Ok(None) => {
            if Instant::now() >= entry.deadline {
                crate::process_tree::terminate_containment(&entry.containment, entry.child.id());
                let _ = entry.child.kill();
                let _ = entry.child.wait();
                if entry.stdout_pipe.is_none() {
                    if let Some(mut out) = entry.child.stdout.take() {
                        let _ = out.read_to_end(&mut entry.stdout);
                    }
                }
                if entry.stderr_pipe.is_none() {
                    if let Some(mut err) = entry.child.stderr.take() {
                        let _ = err.read_to_end(&mut entry.stderr);
                    }
                }
                entry.exit_code = Some(-1);
                entry.finished_ms = Some(now_ms());
                harvest_finished_drains(entry);
            }
        }
        Err(_) => {}
    }
}

pub(crate) fn adopt_running(
    child: Child,
    containment: crate::process_tree::ProcessContainment,
    lang: &str,
    started: Instant,
    stdout_pipe: Option<DrainedPipe>,
    stderr_pipe: Option<DrainedPipe>,
) -> String {
    let started_ms = now_ms().saturating_sub(started.elapsed().as_millis() as u64);
    let id = next_id(started_ms ^ (child.id() as u64));
    let entry = TaskEntry {
        child,
        containment,
        lang: lang.to_string(),
        started_ms,
        deadline: deadline_after(started, ADOPTED_TASK_TIMEOUT_MS)
            .expect("fixed adopted task timeout fits Instant"),
        stdout_pipe,
        stderr_pipe,
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        finished_ms: None,
    };
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.insert(id.clone(), entry);
    drop(reg);
    ensure_reaper_running();
    id
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
    let timeout_ms = params
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000);
    if lang.is_empty() {
        return json!({"ok": false, "error": "lang required"});
    }
    if code.is_empty() {
        return json!({"ok": false, "error": "code required"});
    }
    let Some((cmd, args, stdin_payload)) = crate::exec_js::build_command(lang, code) else {
        return json!({"ok": false, "error": format!("unsupported lang: {lang}")});
    };
    let mut command = Command::new(&cmd);
    command
        .args(&args)
        .current_dir(cwd)
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::exec_js::configure_toolchain_path(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        crate::windowless::apply_windowless(&mut command);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": format!("spawn failed: {e}")}),
    };
    let containment = match crate::process_tree::establish_containment(&child) {
        Ok(containment) => containment,
        Err(error) => {
            crate::process_tree::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return json!({"ok": false, "error": format!("process containment failed: {error}")});
        }
    };
    if let (Some(payload), Some(mut stdin)) = (stdin_payload, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut stdin, payload.as_bytes());
        });
    }
    let stdout_pipe = child.stdout.take().map(drain_in_background);
    let stderr_pipe = child.stderr.take().map(drain_in_background);
    let Some(deadline) = deadline_after(Instant::now(), timeout_ms) else {
        return json!({"ok": false, "error": "timeoutMs is too large"});
    };
    let started = now_ms();
    let id = next_id(started ^ (child.id() as u64));
    let entry = TaskEntry {
        child,
        containment,
        lang: lang.to_string(),
        started_ms: started,
        deadline,
        stdout_pipe,
        stderr_pipe,
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        finished_ms: None,
    };
    let mut reg = registry().lock().unwrap();
    reg.insert(id.clone(), entry);
    drop(reg);
    ensure_reaper_running();
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
    let max_bytes = params
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(65536) as usize;
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
    let stdout = entry
        .stdout_pipe
        .as_ref()
        .map(DrainedPipe::snapshot)
        .unwrap_or_else(|| entry.stdout.clone());
    let stderr = entry
        .stderr_pipe
        .as_ref()
        .map(DrainedPipe::snapshot)
        .unwrap_or_else(|| entry.stderr.clone());
    json!({
        "ok": true,
        "id": id,
        "stdout": tail(&stdout),
        "stderr": tail(&stderr),
        "running": entry.finished_ms.is_none(),
        "exit_code": entry.exit_code,
    })
}

fn stop(params: &Value) -> Value {
    let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return json!({"ok": false, "error": "task id required"});
    }
    let mut entry = {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = reg.remove(id) else {
            return json!({"ok": false, "error": format!("no such task {id}")});
        };
        entry
    };
    crate::process_tree::terminate_containment(&entry.containment, entry.child.id());
    let _ = entry.child.kill();
    let _ = entry.child.wait();
    let drain_deadline = Instant::now() + Duration::from_millis(1500);
    if let Some(pipe) = entry.stdout_pipe.take() {
        entry.stdout = pipe.collect_with_grace(drain_deadline);
    }
    if let Some(pipe) = entry.stderr_pipe.take() {
        entry.stderr = pipe.collect_with_grace(drain_deadline);
    }
    json!({"ok": true, "id": id, "stopped": true})
}

pub fn handle(action: &str, params: &Value, cwd: &Path) -> Value {
    match action {
        "spawn" => spawn(params, cwd),
        "list" => list(),
        "output" => output(params),
        "stop" => stop(params),
        other => json!({"ok": false, "error": format!("unknown task action: {other}")}),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::json;

    #[test]
    fn adopted_deadline_includes_elapsed_foreground_runtime() {
        let _guard = super::test_registry_guard();
        let started = Instant::now() - Duration::from_millis(250);
        assert!(super::deadline_after(started, 200).unwrap() <= Instant::now());
        assert_eq!(
            super::deadline_after(started, super::ADOPTED_TASK_TIMEOUT_MS)
                .unwrap()
                .duration_since(started),
            Duration::from_millis(super::ADOPTED_TASK_TIMEOUT_MS),
        );
    }

    #[test]
    fn task_reaper_deadline_terminates_pipe_holding_descendant_after_shell_exit() {
        let _guard = super::test_registry_guard();
        let spawned = super::handle(
            "spawn",
            &json!({
                "lang": "bash",
                "code": "printf '%s' early; printf '%s' err-early >&2; (sleep 5; printf '%s' late; printf '%s' err-late >&2) &",
                "timeoutMs": 200,
            }),
            Path::new("."),
        );
        assert_eq!(
            spawned.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        let id = spawned
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap()
            .to_string();

        let shell_exit_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let output = super::handle("output", &json!({"id": id}), Path::new("."));
            if output.get("running").and_then(|value| value.as_bool()) == Some(false) {
                assert_eq!(
                    output.get("exit_code").and_then(|value| value.as_i64()),
                    Some(0)
                );
                break;
            }
            assert!(Instant::now() < shell_exit_deadline, "shell did not exit");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(2500));
        let deadline = Instant::now() + Duration::from_secs(2);
        let completed = loop {
            let output = super::handle("output", &json!({"id": id}), Path::new("."));
            let drained = super::registry()
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|entry| entry.stdout_pipe.is_none() && entry.stderr_pipe.is_none());
            if drained {
                break output;
            }
            assert!(
                Instant::now() < deadline,
                "task pipes stayed open after deadline"
            );
            thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(
            completed.get("running").and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            completed.get("exit_code").and_then(|value| value.as_i64()),
            Some(0)
        );
        assert!(completed
            .get("stdout")
            .and_then(|value| value.as_str())
            .unwrap()
            .contains("early"));
        assert!(completed
            .get("stderr")
            .and_then(|value| value.as_str())
            .unwrap()
            .contains("err-early"));
        assert!(!completed
            .get("stdout")
            .and_then(|value| value.as_str())
            .unwrap()
            .contains("late"));
        assert!(!completed
            .get("stderr")
            .and_then(|value| value.as_str())
            .unwrap()
            .contains("err-late"));
        super::registry().lock().unwrap().remove(&id);
    }

    #[test]
    fn task_registry_serves_other_requests_while_a_descendant_keeps_exited_shell_pipes_open() {
        let _guard = super::test_registry_guard();
        let holding = super::handle(
            "spawn",
            &json!({
                "lang": "bash",
                "code": "(sleep 2) &",
                "timeoutMs": 10_000,
            }),
            Path::new("."),
        );
        assert_eq!(
            holding.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        let holding_id = holding
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap()
            .to_string();

        let shell_exit_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let shell_exited = {
                let mut reg = super::registry().lock().unwrap();
                reg.get_mut(&holding_id)
                    .is_some_and(|entry| entry.child.try_wait().ok().flatten().is_some())
            };
            if shell_exited {
                break;
            }
            assert!(Instant::now() < shell_exit_deadline, "shell did not exit");
            thread::sleep(Duration::from_millis(5));
        }

        let list_started = Instant::now();
        let listed = super::handle("list", &json!({}), Path::new("."));
        assert!(
            list_started.elapsed() < Duration::from_millis(750),
            "list waited for open pipes"
        );
        assert_eq!(
            listed.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(listed
            .get("tasks")
            .and_then(|value| value.as_array())
            .is_some_and(|tasks| {
                tasks.iter().any(|task| {
                    task.get("id").and_then(|value| value.as_str()) == Some(holding_id.as_str())
                })
            }));

        let short = super::handle(
            "spawn",
            &json!({
                "lang": "bash",
                "code": "printf '%s' independent",
                "timeoutMs": 5_000
            }),
            Path::new("."),
        );
        assert_eq!(
            short.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        let short_id = short.get("id").and_then(|value| value.as_str()).unwrap();
        let output_deadline = Instant::now() + Duration::from_secs(1);
        let output = loop {
            let output_started = Instant::now();
            let output = super::handle("output", &json!({"id": short_id}), Path::new("."));
            assert!(
                output_started.elapsed() < Duration::from_millis(750),
                "output waited for open pipes"
            );
            if output.get("exit_code").and_then(|value| value.as_i64()) == Some(0)
                && output.get("stdout").and_then(|value| value.as_str()) == Some("independent")
            {
                break output;
            }
            assert!(
                Instant::now() < output_deadline,
                "independent task did not finish"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            output.get("stdout").and_then(|value| value.as_str()),
            Some("independent")
        );
        let stopped = super::handle("stop", &json!({"id": holding_id}), Path::new("."));
        assert_eq!(
            stopped.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn spawned_bash_uses_the_configured_cargo_bin() {
        let _guard = super::test_registry_guard();
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        if !std::path::PathBuf::from(home)
            .join(".cargo")
            .join("bin")
            .join("cargo")
            .is_file()
        {
            return;
        }
        let spawned = super::handle(
            "spawn",
            &json!({"lang": "bash", "code": "cargo --version >/dev/null", "timeoutMs": 5_000}),
            Path::new("."),
        );
        let id = spawned
            .get("id")
            .and_then(|value| value.as_str())
            .expect("task id")
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = super::handle("output", &json!({"id": id}), Path::new("."));
            if output.get("running").and_then(|value| value.as_bool()) == Some(false) {
                assert_eq!(
                    output.get("exit_code").and_then(|value| value.as_i64()),
                    Some(0)
                );
                break;
            }
            assert!(Instant::now() < deadline, "cargo task did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
