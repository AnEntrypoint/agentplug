use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

const RESULT_SENTINEL: &str = "__GM_RESULT__";
const META_SENTINEL: &str = "__GM_META__";
const PROFILE_SENTINEL: &str = "__GM_PROFILE__";
const DEFAULT_LIMIT_MS: u64 = 300_000;
const HARD_CEILING_MS: u64 = 900_000;
const MIN_LIMIT_MS: i64 = 100;

struct BuiltCommand {
    cmd: String,
    args: Vec<String>,
    stdin_payload: Option<String>,
}

fn resolve_limit(opts: &Value) -> Result<(u64, Option<u64>), Value> {
    match opts.get("timeoutMs").and_then(|v| v.as_i64()) {
        Some(ms) if ms < MIN_LIMIT_MS => Err(json!({
            "ok": false, "error": "timeoutMs below floor", "min": MIN_LIMIT_MS, "received": ms,
        })),
        Some(ms) if ms as u64 > HARD_CEILING_MS => Ok((HARD_CEILING_MS, Some(ms as u64))),
        Some(ms) => Ok((ms as u64, None)),
        None => Ok((DEFAULT_LIMIT_MS, None)),
    }
}

fn exec_path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(extra) = std::env::var_os("GM_EXEC_PATH_EXTRA") {
        dirs.extend(std::env::split_paths(&extra).filter(|dir| !dir.as_os_str().is_empty()));
    }
    if let Some(login) = login_shell_path() {
        dirs.extend(std::env::split_paths(&login).filter(|dir| !dir.as_os_str().is_empty()));
    }
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .map(|home| home.join("bin"));
    if let Some(cargo_bin) = cargo_bin.filter(|dir| dir.is_dir()) {
        dirs.push(cargo_bin);
    }
    dirs.extend(
        [
            "/config/tools",
            "/config/workspace/google-cloud-sdk/bin",
            "/config/go-install",
            "/config/.gm-tools",
        ]
        .into_iter()
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir()),
    );
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

fn login_shell_path() -> Option<OsString> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    let output = Command::new(shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let value = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    if value.is_empty() {
        None
    } else {
        Some(OsString::from(value))
    }
}

fn exec_path() -> &'static OsString {
    static EXEC_PATH: OnceLock<OsString> = OnceLock::new();
    EXEC_PATH.get_or_init(|| {
        std::env::join_paths(exec_path_dirs())
            .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
    })
}

pub fn run(code: &str, opts: &Value, cwd: &Path) -> Value {
    let lang = opts
        .get("lang")
        .and_then(|v| v.as_str())
        .unwrap_or("nodejs");
    let (limit_ms, _clamped_from) = match resolve_limit(opts) {
        Ok(v) => v,
        Err(rejection) => return rejection,
    };

    let is_js_lang = lang == "nodejs" || lang == "js";
    let want_profile = opts
        .get("profile")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && is_js_lang;
    let profile_skipped = if opts
        .get("profile")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && !is_js_lang
    {
        Some(json!({
            "reason": format!("profile requested but lang={lang} is not js/nodejs; CPU profiling only supported on the node surface"),
            "lang": lang,
        }))
    } else {
        None
    };
    let want_mem =
        opts.get("mem").and_then(|v| v.as_bool()).unwrap_or(false) && is_js_lang && !want_profile;
    let mode = if want_profile {
        ExecMode::Profile
    } else if want_mem {
        ExecMode::Mem
    } else {
        ExecMode::Default
    };

    let built = match build_command_mode(lang, code, mode, opts) {
        Some(v) => v,
        None => return json!({"ok": false, "error": format!("unsupported lang: {lang}")}),
    };

    let t0 = Instant::now();
    let mut command = Command::new(&built.cmd);
    command
        .args(&built.args)
        .env("PATH", exec_path())
        .current_dir(cwd)
        .stdin(if built.stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_toolchain_path(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        crate::windowless::apply_windowless(&mut command);
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
    let containment = match crate::process_tree::establish_containment(&child) {
        Ok(containment) => containment,
        Err(error) => {
            crate::process_tree::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return json!({
                "ok": false,
                "stdout": "",
                "stderr": error,
                "exit_code": -1,
                "containment_error": true,
            });
        }
    };

    if let (Some(payload), Some(mut stdin)) = (built.stdin_payload, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }
    let stdout_pipe = child.stdout.take().map(crate::task::drain_in_background);
    let stderr_pipe = child.stderr.take().map(crate::task::drain_in_background);

    let waited = child.wait_timeout(Duration::from_millis(limit_ms));
    if matches!(waited, Ok(None)) {
        let task_id =
            crate::task::adopt_running(child, containment, lang, t0, stdout_pipe, stderr_pipe);
        return json!({
            "ok": true,
            "timed_out": true,
            "in_progress": true,
            "task_id": task_id,
            "elapsed_ms": t0.elapsed().as_millis() as u64,
            "task_timeout_ms": 30 * 60 * 1000,
            "decision_required": "this call hit its timeoutMs while work was still running; it remains alive in the task registry as task_id. Poll task-output with {\"id\":\"<task_id>\"} for progress or final output, or call task-stop with that id to kill it. The adopted task is killed if it remains active for 30 minutes."
        });
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    let duration_ms = t0.elapsed().as_millis() as u64;
    let drains_finished = stdout_pipe
        .as_ref()
        .map_or(true, crate::task::DrainedPipe::is_finished)
        && stderr_pipe
            .as_ref()
            .map_or(true, crate::task::DrainedPipe::is_finished);
    if !drains_finished {
        let task_id =
            crate::task::adopt_running(child, containment, lang, t0, stdout_pipe, stderr_pipe);
        return json!({
            "ok": true,
            "timed_out": false,
            "in_progress": true,
            "task_id": task_id,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "task_timeout_ms": 30 * 60 * 1000,
            "decision_required": "the direct process exited but a descendant still owns its output pipes; task-output with {\"id\":\"<task_id>\"} will retain output until every pipe closes. Call task-stop with that id to terminate the process group."
        });
    }
    let stdout_buf = stdout_pipe.map(|p| p.collect()).unwrap_or_default();
    let stderr_buf = stderr_pipe.map(|p| p.collect()).unwrap_or_default();

    let stdout_raw = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    match mode {
        ExecMode::Profile => {
            let (clean_stdout, parsed) = extract_sentinel(&stdout_raw, PROFILE_SENTINEL);
            let ok = exit_code == 0
                && parsed
                    .as_ref()
                    .map(|p| p.get("user_error").map(|e| e.is_null()).unwrap_or(true))
                    .unwrap_or(false);
            let mut v = json!({
                "ok": ok,
                "stdout": clean_stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "timed_out": false,
                "duration_ms": duration_ms,
                "result": parsed.as_ref().and_then(|p| p.get("result")).cloned().unwrap_or(Value::Null),
                "profile": parsed.as_ref().and_then(|p| p.get("profile")).cloned().unwrap_or(json!({"timeframe": null, "culprits": []})),
                "profile_error": parsed.as_ref().and_then(|p| p.get("profile_error")).cloned().unwrap_or_else(|| json!("profile sentinel not found in stdout")),
                "mem": parsed.as_ref().and_then(|p| p.get("mem")).cloned().unwrap_or(Value::Null),
                "wall_vs_cpu": parsed.as_ref().and_then(|p| p.get("wall_vs_cpu")).cloned().unwrap_or(Value::Null),
            });
            if let Some(u) = parsed.as_ref().and_then(|p| p.get("user_error")) {
                if !u.is_null() {
                    v["user_error"] = u.clone();
                }
            }
            v
        }
        ExecMode::Mem => {
            let (clean_stdout, parsed) = extract_sentinel(&stdout_raw, META_SENTINEL);
            let has_error = parsed
                .as_ref()
                .and_then(|p| p.get("error"))
                .map(|e| !e.is_null())
                .unwrap_or(false);
            let ok = exit_code == 0 && parsed.is_some() && !has_error;
            let mut v = json!({
                "ok": ok,
                "stdout": clean_stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "timed_out": false,
                "duration_ms": duration_ms,
                "result": parsed.as_ref().and_then(|p| p.get("result")).cloned().unwrap_or(Value::Null),
                "mem": parsed.as_ref().and_then(|p| p.get("mem")).cloned().unwrap_or(Value::Null),
                "wall_ms": parsed.as_ref().and_then(|p| p.get("wall_ms")).cloned().unwrap_or(Value::Null),
            });
            if has_error {
                v["error"] = parsed
                    .as_ref()
                    .and_then(|p| p.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            v
        }
        ExecMode::Default => {
            let mut stdout = stdout_raw;
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
                "stderr": stderr,
                "exit_code": exit_code,
                "timed_out": false,
                "duration_ms": duration_ms,
            });
            if let Some(r) = result_field {
                v["result"] = r;
            }
            if let Some(skipped) = profile_skipped {
                v["profile_skipped"] = skipped;
            }
            v
        }
    }
}

pub(crate) fn configure_toolchain_path(command: &mut Command) {
    let Some(cargo_bin) = cargo_bin_dir() else {
        return;
    };
    if !cargo_bin.is_dir() {
        return;
    }
    let Some(path) = prepend_path(&cargo_bin, std::env::var_os("PATH")) else {
        return;
    };
    command.env("PATH", path);
}

fn cargo_bin_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    cargo_bin_dir_from_homes(std::env::var_os("CARGO_HOME"), home)
}

fn cargo_bin_dir_from_homes(
    cargo_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(cargo_home) = cargo_home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(cargo_home).join("bin"));
    }
    Some(PathBuf::from(home?).join(".cargo").join("bin"))
}

fn prepend_path(bin: &Path, existing: Option<std::ffi::OsString>) -> Option<std::ffi::OsString> {
    let existing = existing
        .as_ref()
        .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
        .unwrap_or_default();
    if existing.iter().any(|entry| entry == bin) {
        return None;
    }
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(existing)).ok()
}

fn extract_sentinel(stdout: &str, sentinel: &str) -> (String, Option<Value>) {
    match stdout.find(sentinel) {
        Some(idx) => {
            let tail = &stdout[idx + sentinel.len()..];
            let parsed = serde_json::from_str::<Value>(tail).ok();
            (stdout[..idx].to_string(), parsed)
        }
        None => (stdout.to_string(), None),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ExecMode {
    Default,
    Mem,
    Profile,
}

pub(crate) fn build_command(
    lang: &str,
    code: &str,
) -> Option<(String, Vec<String>, Option<String>)> {
    build_command_mode(lang, code, ExecMode::Default, &json!({}))
        .map(|b| (b.cmd, b.args, b.stdin_payload))
}

fn build_command_mode(
    lang: &str,
    code: &str,
    mode: ExecMode,
    opts: &Value,
) -> Option<BuiltCommand> {
    match lang {
        "nodejs" | "js" => {
            let wrapped = match mode {
                ExecMode::Default => {
                    let trimmed = code.trim();
                    let is_bare_expression = !trimmed.is_empty()
                        && !trimmed.contains(';')
                        && !trimmed.contains('\n')
                        && !trimmed.starts_with("return")
                        && !trimmed.starts_with("const ")
                        && !trimmed.starts_with("let ")
                        && !trimmed.starts_with("var ")
                        && !trimmed.starts_with("throw ")
                        && !trimmed.starts_with("if")
                        && !trimmed.starts_with("for")
                        && !trimmed.starts_with("while")
                        && !trimmed.starts_with("switch")
                        && !trimmed.starts_with("try")
                        && !trimmed.starts_with("function")
                        && !trimmed.starts_with("class ")
                        && !trimmed.starts_with("//")
                        && !trimmed.starts_with("/*");
                    let body = if is_bare_expression {
                        format!("    return (\n{code}\n);\n")
                    } else {
                        format!("{code}\n")
                    };
                    format!(
                        "(async () => {{\n  try {{\n    const __r = await (async () => {{\n{body}}})();\n    try {{ console.log('{RESULT_SENTINEL}' + JSON.stringify(__r === undefined ? null : __r)); }}\n    catch (__se) {{ console.log('{RESULT_SENTINEL}' + JSON.stringify({{ __unserializable: String(__se && __se.message || __se) }})); }}\n  }} catch (__e) {{\n    console.error(String(__e && __e.stack || __e));\n    process.exitCode = 1;\n  }}\n}})();\n"
                    )
                },
                ExecMode::Mem => format!(
                    "const {{ performance: __perf }} = require('perf_hooks');\n\
                     (async () => {{\n\
                     \x20 const __mb = process.memoryUsage(); const __w0 = __perf.now();\n\
                     \x20 let __r = null, __err = null;\n\
                     \x20 try {{ __r = await (async () => {{\n{code}\n}})(); }} catch (e) {{ __err = {{ name: e && e.name || 'Error', message: String(e && e.message || e), stack: String(e && e.stack || '') }}; }}\n\
                     \x20 const __wallMs = Math.round((__perf.now() - __w0) * 1000) / 1000; const __ma = process.memoryUsage();\n\
                     \x20 const __mem = {{ rss_mb: Math.round(__ma.rss/10485.76)/100, heapUsed_mb: Math.round(__ma.heapUsed/10485.76)/100, heapUsed_delta_mb: Math.round((__ma.heapUsed-__mb.heapUsed)/10485.76)/100, external_mb: Math.round(__ma.external/10485.76)/100 }};\n\
                     \x20 process.stdout.write('{META_SENTINEL}' + JSON.stringify({{ result: __r === undefined ? null : __r, error: __err, mem: __mem, wall_ms: __wallMs }}));\n\
                     \x20 if (__err) process.exitCode = 1;\n\
                     }})();\n"
                ),
                ExecMode::Profile => {
                    let sample_interval = opts.get("sampleIntervalUs").and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(100);
                    let top_n = opts.get("profileTopN").and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(20);
                    format!(
                        "{AGGREGATE_CPU_PROFILE_SRC}\n\
                         const __inspector = require('inspector');\n\
                         const {{ performance: __perf }} = require('perf_hooks');\n\
                         const __session = new __inspector.Session();\n\
                         __session.connect();\n\
                         const __post = (m, p) => new Promise((res, rej) => __session.post(m, p || {{}}, (e, r) => e ? rej(e) : res(r)));\n\
                         (async () => {{\n\
                         \x20 let __profile = null, __profileError = null, __userResult = null, __userError = null, __wallMs = 0;\n\
                         \x20 const __memBefore = process.memoryUsage();\n\
                         \x20 try {{\n\
                         \x20\x20  await __post('Profiler.enable');\n\
                         \x20\x20  await __post('Profiler.setSamplingInterval', {{ interval: {sample_interval} }});\n\
                         \x20\x20  await __post('Profiler.start');\n\
                         \x20\x20  const __w0 = __perf.now();\n\
                         \x20\x20  try {{ __userResult = await (async () => {{\n{code}\n}})(); }} catch (ue) {{ __userError = String(ue && ue.stack || ue); }}\n\
                         \x20\x20  __wallMs = Math.round((__perf.now() - __w0) * 1000) / 1000;\n\
                         \x20\x20  const __r = await __post('Profiler.stop');\n\
                         \x20\x20  __profile = __r && __r.profile || null;\n\
                         \x20 }} catch (pe) {{ __profileError = String(pe && pe.message || pe); }}\n\
                         \x20 const __memAfter = process.memoryUsage();\n\
                         \x20 const __agg = __profile ? aggregateCpuProfile(__profile, {top_n}, false) : {{ timeframe: null, culprits: [] }};\n\
                         \x20 const __cpuTotalUs = __agg.timeframe ? __agg.timeframe.total_us : 0;\n\
                         \x20 const __wallUs = Math.round(__wallMs * 1000);\n\
                         \x20 const __mem = {{ rss_mb: Math.round(__memAfter.rss/10485.76)/100, heapUsed_mb: Math.round(__memAfter.heapUsed/10485.76)/100, heapUsed_delta_mb: Math.round((__memAfter.heapUsed-__memBefore.heapUsed)/10485.76)/100, external_mb: Math.round(__memAfter.external/10485.76)/100 }};\n\
                         \x20 const __wallVsCpu = {{ wall_us: __wallUs, cpu_total_sampled_us: __cpuTotalUs, offcpu_us: Math.max(0, __wallUs - __cpuTotalUs), note: 'offcpu_us = inner wall minus on-CPU sampled JS self time = IO/async/GPU/idle the CPU sampler is blind to' }};\n\
                         \x20 process.stdout.write('{PROFILE_SENTINEL}' + JSON.stringify({{ result: __userResult, user_error: __userError, profile: __agg, profile_error: __profileError, mem: __mem, wall_vs_cpu: __wallVsCpu }}));\n\
                         \x20 __session.disconnect();\n\
                         }})();\n"
                    )
                }
            };
            let cmd = resolve_node_cmd();
            if command_is_node(&cmd) {
                Some(BuiltCommand {
                    cmd,
                    args: vec!["-".to_string()],
                    stdin_payload: Some(wrapped),
                })
            } else {
                Some(BuiltCommand {
                    cmd,
                    args: vec!["-e".to_string(), wrapped],
                    stdin_payload: None,
                })
            }
        }
        "python" | "py" => Some(BuiltCommand {
            cmd: "python".to_string(),
            args: vec!["-c".to_string(), code.to_string()],
            stdin_payload: None,
        }),
        "bash" | "sh" | "shell" => Some(BuiltCommand {
            cmd: resolve_bash_cmd(),
            args: vec!["-c".to_string(), code.to_string()],
            stdin_payload: None,
        }),
        "powershell" | "ps1" => Some(BuiltCommand {
            cmd: "powershell".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                code.to_string(),
            ],
            stdin_payload: None,
        }),
        "deno" => Some(BuiltCommand {
            cmd: "deno".to_string(),
            args: vec!["eval".to_string(), code.to_string()],
            stdin_payload: None,
        }),
        _ => None,
    }
}

const AGGREGATE_CPU_PROFILE_SRC: &str = r#"function aggregateCpuProfile(profile, topN, isBrowserCtx) {
  if (!profile || !Array.isArray(profile.nodes) || !Array.isArray(profile.samples)) {
    return { timeframe: null, culprits: [] };
  }
  const byId = new Map();
  for (const node of profile.nodes) byId.set(node.id, node);
  const deltas = Array.isArray(profile.timeDeltas) ? profile.timeDeltas : [];
  const selfUs = new Map();
  const sampleCount = profile.samples.length;
  for (let i = 0; i < profile.samples.length; i++) {
    const node = byId.get(profile.samples[i]);
    if (!node) continue;
    const delta = deltas[i + 1] || deltas[i] || 0;
    selfUs.set(node.id, (selfUs.get(node.id) || 0) + Math.abs(delta));
  }
  const totalUs = Array.from(selfUs.values()).reduce((a, b) => a + b, 0);
  const acc = new Map();
  for (const [id, us] of selfUs.entries()) {
    const node = byId.get(id);
    if (!node || !node.callFrame) continue;
    const cf = node.callFrame;
    const fn = cf.functionName || '(anonymous)';
    const loc = `${cf.url || ''}:${cf.lineNumber != null ? cf.lineNumber + 1 : 0}:${cf.columnNumber != null ? cf.columnNumber + 1 : 0}`;
    const key = `${fn}@${loc}`;
    const prior = acc.get(key) || { location: loc, function: fn, self_us: 0, hits: 0 };
    prior.self_us += us;
    prior.hits += 1;
    acc.set(key, prior);
  }
  const culprits = Array.from(acc.values())
    .map(c => ({ ...c, self_pct: totalUs > 0 ? Math.round((c.self_us / totalUs) * 10000) / 100 : 0 }))
    .sort((a, b) => b.self_us - a.self_us)
    .slice(0, topN);
  return {
    timeframe: {
      start_us: typeof profile.startTime === 'number' ? profile.startTime : 0,
      end_us: typeof profile.endTime === 'number' ? profile.endTime : 0,
      total_us: totalUs,
      sample_count: sampleCount,
    },
    culprits,
  };
}"#;

fn command_is_node(cmd: &str) -> bool {
    Path::new(cmd)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("node"))
}

fn resolve_node_cmd() -> String {
    for candidate in ["node", "bun"] {
        if let Some(p) = which(candidate) {
            return p.to_string_lossy().into_owned();
        }
    }
    "node".to_string()
}

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
    which("bash")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bash".to_string())
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_name = if cfg!(windows) {
        format!("{cmd}.exe")
    } else {
        cmd.to_string()
    };
    std::env::split_paths(&path_var)
        .map(|p| p.join(&exe_name))
        .find(|p| p.exists())
}

#[allow(dead_code)]
fn write_script(prefix: &str, content: &str) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}.js", std::process::id()));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    #[test]
    fn foreground_timeout_hands_live_pipes_to_task_until_complete() {
        let _guard = crate::task::test_registry_guard();
        let result = super::run(
            "printf '%s' before-; head -c 131072 /dev/zero | tr '\\000' x; printf '%s' -middle-; printf '%s' err-before- >&2; sleep 0.2; printf '%s' after; printf '%s' err-after >&2; (sleep 0.5; printf '%s' tail; printf '%s' err-tail >&2) &",
            &json!({"lang": "bash", "timeoutMs": 100}),
            Path::new("."),
        );
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            result.get("timed_out").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            result.get("in_progress").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            result.get("task_timeout_ms").and_then(|v| v.as_u64()),
            Some(30 * 60 * 1000)
        );
        let id = result
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("handoff task id")
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_finished_before_drains = false;
        let final_output = loop {
            let output = crate::task::handle(
                "output",
                &json!({"id": id, "max_bytes": 200_000}),
                Path::new("."),
            );
            let finished = output.get("running").and_then(|v| v.as_bool()) == Some(false);
            let stdout = output
                .get("stdout")
                .and_then(|v| v.as_str())
                .expect("stdout");
            let stderr = output
                .get("stderr")
                .and_then(|v| v.as_str())
                .expect("stderr");
            if finished && !stdout.contains("tail") && !stderr.contains("err-tail") {
                saw_finished_before_drains = true;
            }
            if finished && stdout.contains("tail") && stderr.contains("err-tail") {
                break output;
            }
            assert!(Instant::now() < deadline, "adopted task did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(saw_finished_before_drains);
        let stdout = final_output
            .get("stdout")
            .and_then(|v| v.as_str())
            .expect("stdout");
        let stderr = final_output
            .get("stderr")
            .and_then(|v| v.as_str())
            .expect("stderr");
        assert!(stdout.starts_with("before-"));
        assert!(stdout.contains("-middle-aftertail"));
        assert_eq!(stdout.matches('x').count(), 131072);
        assert_eq!(stderr, "err-before-err-aftererr-tail");
        assert_eq!(
            final_output.get("exit_code").and_then(|v| v.as_i64()),
            Some(0)
        );
    }

    #[test]
    fn exited_foreground_child_hands_late_descendant_output_to_task() {
        let _guard = crate::task::test_registry_guard();
        let result = super::run(
            "printf '%s' before-; printf '%s' err-before- >&2; (sleep 0.5; printf '%s' tail; printf '%s' err-tail >&2) &",
            &json!({"lang": "bash", "timeoutMs": 1_000}),
            Path::new("."),
        );
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            result.get("timed_out").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            result.get("in_progress").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(result.get("exit_code").and_then(|v| v.as_i64()), Some(0));
        let id = result
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("handoff task id")
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_finished_before_drains = false;
        let final_output = loop {
            let output = crate::task::handle("output", &json!({"id": id}), Path::new("."));
            let finished = output.get("running").and_then(|v| v.as_bool()) == Some(false);
            let stdout = output
                .get("stdout")
                .and_then(|v| v.as_str())
                .expect("stdout");
            let stderr = output
                .get("stderr")
                .and_then(|v| v.as_str())
                .expect("stderr");
            if finished && !stdout.contains("tail") && !stderr.contains("err-tail") {
                saw_finished_before_drains = true;
            }
            if finished && stdout.contains("tail") && stderr.contains("err-tail") {
                break output;
            }
            assert!(Instant::now() < deadline, "late descendant output was lost");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(saw_finished_before_drains);
        assert_eq!(
            final_output.get("stdout").and_then(|v| v.as_str()),
            Some("before-tail")
        );
        assert_eq!(
            final_output.get("stderr").and_then(|v| v.as_str()),
            Some("err-before-err-tail")
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stopping_exited_shell_kills_its_delayed_pipe_owner_without_waiting_for_it() {
        let _guard = crate::task::test_registry_guard();
        let result = super::run(
            "printf '%s' before-; (sleep 5; printf '%s' tail) &",
            &json!({"lang": "bash", "timeoutMs": 1_000}),
            Path::new("."),
        );
        let id = result
            .get("task_id")
            .and_then(|v| v.as_str())
            .expect("handoff task id")
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let output = crate::task::handle("output", &json!({"id": id}), Path::new("."));
            if output.get("running").and_then(|v| v.as_bool()) == Some(false) {
                break;
            }
            assert!(Instant::now() < deadline, "shell did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        let stopped = crate::task::handle("stop", &json!({"id": id}), Path::new("."));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "stop waited for the descendant pipe owner"
        );
        assert_eq!(stopped.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn prepends_cargo_bin_without_losing_caller_path_entries() {
        let cargo_bin = std::path::PathBuf::from("configured-cargo").join("bin");
        let caller = vec![
            std::path::PathBuf::from("caller-one"),
            std::path::PathBuf::from("caller-two"),
        ];
        let path =
            super::prepend_path(&cargo_bin, Some(std::env::join_paths(&caller).unwrap())).unwrap();
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            [cargo_bin, caller[0].clone(), caller[1].clone()]
        );
    }

    #[test]
    fn does_not_duplicate_cargo_bin_in_caller_path() {
        let cargo_bin = std::path::PathBuf::from("configured-cargo").join("bin");
        let caller =
            std::env::join_paths([cargo_bin.clone(), std::path::PathBuf::from("caller")]).unwrap();
        assert!(super::prepend_path(&cargo_bin, Some(caller)).is_none());
    }

    #[test]
    #[cfg(not(windows))]
    fn foreground_bash_uses_the_configured_cargo_bin() {
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
        let result = super::run(
            "cargo --version >/dev/null",
            &json!({"lang": "bash", "timeoutMs": 5_000}),
            Path::new("."),
        );
        assert_eq!(
            result.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn explicit_cargo_home_takes_precedence_over_home() {
        let cargo_home = std::ffi::OsString::from("configured-cargo-home");
        let home = std::ffi::OsString::from("fallback-home");
        assert_eq!(
            super::cargo_bin_dir_from_homes(Some(cargo_home), Some(home)),
            Some(std::path::PathBuf::from("configured-cargo-home").join("bin"))
        );
    }
}
