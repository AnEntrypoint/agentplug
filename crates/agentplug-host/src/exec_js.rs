use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wait_timeout::ChildExt;

const RESULT_SENTINEL: &str = "__GM_RESULT__";
const META_SENTINEL: &str = "__GM_META__";
const PROFILE_SENTINEL: &str = "__GM_PROFILE__";

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
    let want_profile = opts.get("profile").and_then(|v| v.as_bool()).unwrap_or(false) && is_js_lang;
    let profile_skipped = if opts.get("profile").and_then(|v| v.as_bool()).unwrap_or(false) && !is_js_lang {
        Some(json!({
            "reason": format!("profile requested but lang={lang} is not js/nodejs; CPU profiling only supported on the node surface"),
            "lang": lang,
        }))
    } else {
        None
    };
    let want_mem = opts.get("mem").and_then(|v| v.as_bool()).unwrap_or(false) && is_js_lang && !want_profile;
    let mode = if want_profile {
        ExecMode::Profile
    } else if want_mem {
        ExecMode::Mem
    } else {
        ExecMode::Default
    };

    let (cmd, args, script_file) = match build_command_mode(lang, code, mode, opts) {
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

    let still_running = matches!(child.wait_timeout(Duration::from_millis(timeout_ms)), Ok(None));
    if still_running {
        let task_id = crate::task::adopt_running(child, lang, t0, timeout_ms);
        return json!({
            "ok": true,
            "timed_out": true,
            "in_progress": true,
            "task_id": task_id,
            "elapsed_ms": t0.elapsed().as_millis() as u64,
            "decision_required": "this call hit its timeoutMs still running -- it was NOT killed, it is alive in the background task registry as task_id. Decide: `task-output {id}` to keep it running and poll progress/result later (the queue is already free, this worker returned immediately), or `task-stop {id}` to kill it now. It does not run forever unattended -- dispatch one of those two, do not leave it un-decided.",
        });
    }

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

    let stdout_raw = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    match mode {
        ExecMode::Profile => {
            let (clean_stdout, parsed) = extract_sentinel(&stdout_raw, PROFILE_SENTINEL);
            let ok = exit_code == 0 && parsed.as_ref().map(|p| p.get("user_error").map(|e| e.is_null()).unwrap_or(true)).unwrap_or(false);
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
            let has_error = parsed.as_ref().and_then(|p| p.get("error")).map(|e| !e.is_null()).unwrap_or(false);
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
                v["error"] = parsed.as_ref().and_then(|p| p.get("error")).cloned().unwrap_or(Value::Null);
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

pub(crate) fn build_command(lang: &str, code: &str) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    build_command_mode(lang, code, ExecMode::Default, &json!({}))
}

fn build_command_mode(
    lang: &str,
    code: &str,
    mode: ExecMode,
    opts: &Value,
) -> Option<(String, Vec<String>, Option<std::path::PathBuf>)> {
    match lang {
        "nodejs" | "js" => {
            let wrapped = match mode {
                ExecMode::Default => format!(
                    "(async () => {{\n  try {{\n    const __r = await (async () => {{\n{code}\n}})();\n    try {{ console.log('{RESULT_SENTINEL}' + JSON.stringify(__r === undefined ? null : __r)); }}\n    catch (__se) {{ console.log('{RESULT_SENTINEL}' + JSON.stringify({{ __unserializable: String(__se && __se.message || __se) }})); }}\n  }} catch (__e) {{\n    console.error(String(__e && __e.stack || __e));\n    process.exitCode = 1;\n  }}\n}})();\n"
                ),
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
