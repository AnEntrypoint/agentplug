use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
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
//
// Live-found (browser-session-process-not-persisted): a fresh Chrome used to
// launch AND get unconditionally killed at the end of EVERY single dispatch,
// so only the on-disk --user-data-dir profile persisted across dispatches --
// the live process/visible window never did, meaning "open the browser so we
// can see it" flashed open and immediately closed. Fixed: a process-wide
// SESSIONS registry (below) now keeps a launched Chrome child + its CDP port
// alive across dispatches sharing the same (cwd, session_id) key, reusing it
// on every subsequent dispatch instead of relaunching. The Chrome process now
// only dies via an explicit `session close`/`session reset`, the opportunistic
// idle-timeout reaper, or the whole agentplug-runner daemon exiting (normal
// OS child-process teardown).

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
    // Live-found (user-reported): Chrome launched with a hardcoded
    // --headless=new on every dispatch, no way to opt into a visible window
    // even though this project's own stated preference is headful Chrome.
    // Default false (headful) -- the opposite of the previous hardcoded
    // behavior -- matching the explicit stated preference; a project that
    // genuinely wants headless (CI, no display attached) sets
    // {"headless": true} in .gm/browser-config.json.
    #[serde(default)]
    headless: Option<bool>,
    // Live-found (browser-session-process-not-persisted): sessions now keep
    // their Chrome process alive indefinitely across dispatches unless
    // explicitly closed, so an idle-timeout reaper is needed to avoid
    // accumulating abandoned Chrome processes across a long-lived daemon.
    // Default 30 minutes when absent.
    #[serde(default)]
    session_idle_timeout_ms: Option<u64>,
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
                headless: None,
                session_idle_timeout_ms: None,
            })
    }
    fn cdp_poll_timeout(&self) -> Duration { Duration::from_millis(self.cdp_poll_timeout_ms.unwrap_or(1000)) }
    fn cdp_poll_interval(&self) -> Duration { Duration::from_millis(self.cdp_poll_interval_ms.unwrap_or(250)) }
    fn chrome_ready_deadline(&self) -> Duration { Duration::from_millis(self.chrome_ready_deadline_ms.unwrap_or(30_000)) }
    fn eval_timeout_grace(&self) -> u64 { self.eval_timeout_grace_ms.unwrap_or(6000) }
    fn headless(&self) -> bool { self.headless.unwrap_or(false) }
    fn session_idle_timeout(&self) -> Duration {
        Duration::from_millis(self.session_idle_timeout_ms.unwrap_or(30 * 60 * 1000))
    }
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

/// Live-found (chrome-discovery-misses-macos): this list only ever checked
/// `/usr/bin/google-chrome`+chromium (Linux) or the Windows Program Files
/// paths -- a standard macOS install at
/// `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` was never
/// a candidate, so `find_chrome()` fell straight through to the `which()`
/// PATH lookup below, which fails too (installing Chrome.app does not put
/// anything on PATH), and every browser dispatch on an otherwise-correctly-
/// provisioned Mac failed with "no Chrome found; install Google Chrome or
/// Chromium". Fixed by adding the standard system-wide and per-user
/// macOS app-bundle paths (plus Chromium.app for parity with the Linux
/// chromium fallback) as additional candidates -- the existing Linux/
/// Windows/PATH checks are untouched since this same binary is built and
/// run on those platforms too.
fn find_chrome() -> Option<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        let mut v = vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            v.push(
                PathBuf::from(home)
                    .join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            );
        }
        v
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
/// path (only the retired JS wrapper had them). Stripped (alongside
/// `strip_timeout_prefix`, in a loop so the two compose in either order)
/// before parse_body sees the remaining url=/bare-URL/script body, so all
/// three prefix systems compose (e.g. "profile\nurl=https://...\nscript",
/// or "timeout=30000\nurl=https://...\nscript", or both stacked together).
#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(test, derive(Debug))]
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

/// Live-found (timeout-prefix-stacking-syntax-error): `timeout=<ms>` was
/// documented as a stackable dispatch prefix (e.g.
/// `timeout=30000\nurl=http://host/\n<script>`) but NOTHING in this file
/// ever recognized it -- `strip_mode_prefix` only knows capture/profile/
/// trace, and `parse_body` only recognizes `url=`/a bare `http(s)://` URL
/// as literally the body's first token. So a body starting with
/// `timeout=...` fell through every check and the ENTIRE remaining text --
/// including the real `url=...` line after it -- was handed to Node as the
/// script verbatim. `url=http://x/` is not valid JS as a bare statement:
/// it tokenizes as the assignment expression `url = http` (the `//` that
/// follows is lexed as a line comment, eating `/x/` and the rest of the
/// line) immediately followed by a stray `:` left over from `http:`, which
/// V8 rejects with exactly the reported `SyntaxError: Unexpected token
/// ':'` -- instantly, since no `startUrl` was ever extracted, so
/// `evalOnly` never even reaches its `Page.navigate` branch. Fixed by
/// giving `timeout=` its own prefix-stripper, called in a loop alongside
/// `strip_mode_prefix` in `run()` below so mode and timeout compose in
/// EITHER order before `parse_body` ever sees the (now-correctly-located)
/// `url=`/script remainder -- matching how mode already composed with
/// url=, just extended to cover this second, previously entirely
/// unimplemented prefix.
fn strip_timeout_prefix(body: &str) -> (Option<u64>, &str) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("timeout=") {
        let (num, remainder) = match rest.find('\n') {
            Some(nl) => (&rest[..nl], &rest[nl + 1..]),
            None => (rest, ""),
        };
        if let Ok(ms) = num.trim().parse::<u64>() {
            return (Some(ms), remainder);
        }
    }
    (None, body)
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
/// persists across a debugging session. As of the SESSIONS registry below,
/// the live Chrome PROCESS itself also now persists across dispatches, not
/// just this on-disk directory -- this dir survives even a `session close`
/// (only the process is killed) so the next `session new` under the same id
/// still gets a warm profile. `sanitize` keeps the directory name
/// filesystem-safe the same way the old temp-dir stamp already did.
fn browser_chrome_profile_dir(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(".gm").join(format!("browser-chrome-profile-{}", sanitize(session_id)))
}

/// A live, registry-tracked Chrome process for one `(cwd, session_id)` key.
struct BrowserSession {
    cwd: PathBuf,
    session_id: String,
    child: Child,
    port: u16,
    last_used: Instant,
}

/// Process-wide registry of live Chrome sessions, keyed by a String formed
/// from `(cwd, session_id)` so two different projects reusing the same bare
/// session_id never collide/share a Chrome process. Mirrors the house
/// pattern in agentplug-runner's daemon.rs (`static IN_FLIGHT: OnceLock<
/// Mutex<HashMap<...>>>`).
static SESSIONS: OnceLock<Mutex<HashMap<String, BrowserSession>>> = OnceLock::new();

fn sessions_map() -> &'static Mutex<HashMap<String, BrowserSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_key(cwd: &Path, session_id: &str) -> String {
    format!("{}\u{0}{}", cwd.display(), session_id)
}

/// Non-blocking liveness check: `try_wait() == Ok(None)` means still running.
/// A session whose process already exited on its own (user closed the
/// window manually, Chrome crashed) is treated as dead so callers fall
/// through to a fresh relaunch instead of reusing a corpse entry.
fn session_is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

fn kill_session(mut session: BrowserSession) {
    let _ = session.child.kill();
    let _ = session.child.wait();
    let _ = std::fs::remove_file(pid_sidecar_path(&browser_chrome_profile_dir(&session.cwd, &session.session_id)));
}

/// Sidecar file recording the OS pid of the Chrome process launched into a
/// given profile dir, so a LATER process (this same binary restarted after a
/// crash, or a fresh daemon after a lost-heartbeat race) can identify and
/// reap an orphan even though its own in-memory `SESSIONS` registry starts
/// empty. Written at launch, removed on any clean kill path
/// (`kill_session`); a leftover file with a since-reused or dead pid is
/// exactly the crash-orphan signal `reap_os_orphans` looks for.
fn pid_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("chrome.pid")
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            // tasklist prints a matching CSV row ("name","pid",...) when the
            // pid exists, or an "INFO: No tasks..." line (no comma) when it
            // does not -- a comma in the first line is the cheap
            // discriminator (same pattern as agentplug-runner's daemon.rs
            // is_daemon_fresh liveness check).
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        // tasklist itself failing to run is a host-environment problem, not
        // evidence the pid is dead -- fail open (treat as alive, i.e. do
        // NOT kill) rather than false-positive-reap a process tasklist
        // merely couldn't be asked about.
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence/permission without sending a real
    // signal -- the POSIX-standard liveness probe, zero new dependencies.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

/// Real OS-level orphan reaper, run opportunistically on every `run()` call
/// (same no-dedicated-timer-thread pattern as `reap_idle_sessions`), scoped
/// to this cwd's OWN `.gm/browser-chrome-profile-*` directories only.
///
/// Closes the gap this file's own module doc + browser.md both name: a
/// crash or hard kill (SIGKILL / Windows TerminateProcess / an OOM-killer
/// hit / the whole agentplug-runner process itself being force-killed)
/// leaves a live Chrome child running with NO chance for `close_all_sessions`
/// or `kill_session` to run first -- the in-memory `SESSIONS` registry that
/// tracked it is gone the moment the owning process dies, so a subsequent
/// process (this same daemon restarted, or the next dispatch's `run()` in a
/// freshly-booted daemon after a lost-heartbeat exit) starts with an EMPTY
/// registry and has no way to know that chrome.exe is still alive, using the
/// port, holding the profile-dir lock. Previously nothing ever reaped this
/// case -- only `reap_idle_sessions` (needs a registry entry to time out) and
/// `close_all_sessions` (only runs on a VOLUNTARY exit that has time to call
/// it) existed, neither of which fires for a crash.
///
/// Mechanism: every launched Chrome writes its pid to
/// `<profile_dir>/chrome.pid` at launch (see `launch_chrome`) and removes it
/// on any clean `kill_session`. On each `run()` call, scan this cwd's
/// `.gm/browser-chrome-profile-*` dirs; for each one NOT already claimed by
/// a live entry in `SESSIONS` (i.e. this process has no in-memory record of
/// owning it), read the sidecar pid and check real OS liveness. A dead pid
/// (or unreadable sidecar) just gets the stale sidecar file removed --
/// nothing to reap. A LIVE pid with no owning session is the genuine orphan:
/// kill it (real OS process, not merely a registry entry -- see
/// `kill(pid)`/`taskkill` below) and remove the sidecar so the profile dir
/// is clean for the next `session new`/relaunch.
fn reap_os_orphans(cwd: &Path) {
    let dir = browser_profiles_root_for_orphan_scan(cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let claimed_dirs: std::collections::HashSet<PathBuf> = {
        let map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .filter(|s| s.cwd == cwd)
            .map(|s| browser_chrome_profile_dir(&s.cwd, &s.session_id))
            .collect()
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with("browser-chrome-profile-") {
            continue;
        }
        if claimed_dirs.contains(&path) {
            // A live session in THIS process already owns this profile dir
            // -- nothing to reap regardless of what the sidecar says.
            continue;
        }
        let sidecar = pid_sidecar_path(&path);
        // Grace period, keyed off the sidecar file's own mtime rather than a
        // wall-clock timestamp payload: a chrome that is still mid-launch in
        // ANOTHER process (a self-update handoff briefly overlapping the old
        // and new daemon, both scanning the same .gm/ tree) has just written
        // this file but has not yet had a chance to become CDP-ready and get
        // inserted into ITS OWN process's SESSIONS map -- to this scanning
        // process it looks identical to a real orphan (live pid, unclaimed
        // dir) unless freshly-written sidecars are given a moment to either
        // finish launching (and get genuinely claimed elsewhere) or fail.
        // Matches browser.md's documented "just-launched browser has a grace
        // period before any reaper can touch it" contract.
        const ORPHAN_REAP_GRACE: Duration = Duration::from_secs(15);
        if let Ok(meta) = std::fs::metadata(&sidecar) {
            if let Ok(age) = meta.modified().and_then(|m| m.elapsed().map_err(|e| std::io::Error::other(e))) {
                if age < ORPHAN_REAP_GRACE {
                    continue;
                }
            }
        }
        let Ok(raw) = std::fs::read_to_string(&sidecar) else { continue };
        let Ok(pid) = raw.trim().parse::<u32>() else {
            let _ = std::fs::remove_file(&sidecar);
            continue;
        };
        if pid_is_alive(pid) {
            eprintln!(
                "[agentplug browser] reaping OS-orphaned chrome pid={} (profile {}, no owning session in this process -- crash/hard-exit orphan)",
                pid,
                path.display()
            );
            kill_pid(pid);
        }
        let _ = std::fs::remove_file(&sidecar);
    }
}

fn browser_profiles_root_for_orphan_scan(cwd: &Path) -> PathBuf {
    cwd.join(".gm")
}

/// Bounded liveness re-check against the SAME page context an eval just
/// timed out on, run over the same CDP mechanism (`cdp_eval.js`'s node
/// helper) rather than reimplementing a CDP websocket client in Rust.
/// Trivial script (`1+1`), short fixed deadline (not the caller's own
/// timeout -- this IS the "is it actually wedged" probe, so it must resolve
/// quickly regardless of how long the original eval's timeout was) --
/// returns true only if the page answers with the expected result, false
/// for any timeout/crash/error, which is exactly the "renderer is wedged"
/// signal `run()`'s eval-timeout handler escalates on.
fn session_liveness_recheck(port: u16, browser_cfg: &BrowserConfig) -> bool {
    let Some(node) = which("node") else { return true }; // fail open: can't check, don't punish the session for it
    let tmp = std::env::temp_dir();
    let stamp = format!("{}-livecheck-{}", std::process::id(), unix_ms());
    let helper_path = tmp.join(format!("agentplug-cdp-eval-{stamp}.mjs"));
    let script_path = tmp.join(format!("agentplug-cdp-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-cdp-result-{stamp}.json"));
    if std::fs::write(&helper_path, CDP_EVAL_JS.as_bytes()).is_err() {
        return true;
    }
    if std::fs::write(&script_path, b"return 1+1;").is_err() {
        cleanup(&[&helper_path, &script_path]);
        return true;
    }
    let recheck_timeout_ms: u64 = 5000;
    let cfg = json!({
        "port": port,
        "startUrl": Value::Null,
        "scriptFile": script_path.to_string_lossy(),
        "resultFile": result_path.to_string_lossy(),
        "timeoutMs": recheck_timeout_ms,
        "mode": "default",
        "artifactFile": Value::Null,
    })
    .to_string();
    let spawn = Command::new(&node)
        .arg(&helper_path)
        .arg(&cfg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let alive = match spawn {
        Ok(mut child) => {
            let grace = Duration::from_millis(recheck_timeout_ms + browser_cfg.eval_timeout_grace());
            match child.wait_timeout(grace) {
                Ok(Some(status)) if status.success() => {
                    // Default-mode cdp_eval.js writes the bare returned value
                    // to resultFile (no {result:...} envelope -- that
                    // envelope shape is only used by capture/profile/trace
                    // modes), so the expected success payload here is the
                    // bare number 2, not a wrapped object.
                    let v: Option<Value> = std::fs::read_to_string(&result_path)
                        .ok()
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
                    matches!(v, Some(v) if v == json!(2))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    false
                }
            }
        }
        Err(_) => true, // spawn failure is this recheck's own problem, not evidence the browser is wedged
    };
    cleanup(&[&helper_path, &script_path, &result_path]);
    alive
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F", "/T"]).output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

/// Kills EVERY live session across EVERY project, process-wide -- for the
/// daemon's own voluntary self-update handoff (`attempt_self_update_handoff`
/// in agentplug-runner's daemon.rs), which hands ownership to a freshly
/// spawned process with its own EMPTY `SESSIONS` registry. Without this, a
/// session's Chrome process (a real OS child, unaffected by the handoff on
/// its own) survives as an orphan the NEW process has no record of -- alive,
/// but unreachable via `session list`/`session close`, invisible to the
/// idle-timeout reaper (which only ever reaps what it can see), and never
/// cleaned up short of someone manually killing chrome.exe. A crash/hard
/// exit still orphans sessions the same way (unavoidable -- nothing runs on
/// a process that's already gone), but a VOLUNTARY exit like a self-update
/// handoff has no such excuse: it can and must close what it knows about
/// first. Live-witnessed the orphan this closes: a self-update handoff
/// during this feature's own testing left a `chrome.exe` running with zero
/// tracking in the new daemon process.
pub fn close_all_sessions() {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in keys {
        if let Some(session) = map.remove(&k) {
            eprintln!("[agentplug browser] closing session {} for handoff/shutdown", session.session_id);
            kill_session(session);
        }
    }
}

/// Opportunistic idle-timeout reap, run on every `run()` call (no dedicated
/// timer thread, matching agentplug-runner daemon.rs's PLUGIN_IDLE_EVICT_MS
/// precedent of checking per-tick rather than on its own timer). Only reaps
/// entries belonging to THIS cwd -- other projects' sessions are untouched
/// by a dispatch against this one.
fn reap_idle_sessions(cwd: &Path, cfg: &BrowserConfig) {
    let timeout = cfg.session_idle_timeout();
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let dead_keys: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.cwd == cwd && s.last_used.elapsed() > timeout)
        .map(|(k, _)| k.clone())
        .collect();
    for k in dead_keys {
        if let Some(session) = map.remove(&k) {
            eprintln!(
                "[agentplug browser] reaping idle session {} (idle {}ms > {}ms)",
                session.session_id,
                session.last_used.elapsed().as_millis(),
                timeout.as_millis()
            );
            kill_session(session);
        }
    }
}

/// `session new` -- explicit reset-and-relaunch: kills any existing live
/// session for this key first (if present), then launches a fresh Chrome
/// and registers it, WITHOUT running any script.
fn session_new(cwd: &Path, session_id: &str, cfg: &BrowserConfig) -> Value {
    let key = session_key(cwd, session_id);
    {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.remove(&key) {
            kill_session(existing);
        }
    }
    match launch_chrome(cwd, session_id, cfg) {
        Ok((child, port)) => {
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                key,
                BrowserSession {
                    cwd: cwd.to_path_buf(),
                    session_id: session_id.to_string(),
                    child,
                    port,
                    last_used: Instant::now(),
                },
            );
            json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": session_id, "port": port})
        }
        Err(e) => json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e}),
    }
}

/// `session list` -- live sessions for THIS cwd only, lazily pruning any
/// entry whose process has since died (opportunistic cleanup alongside the
/// timeout reaper, not a replacement for it).
fn session_list(cwd: &Path) -> Value {
    let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
    let keys_for_cwd: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.cwd == cwd)
        .map(|(k, _)| k.clone())
        .collect();
    let mut out = Vec::new();
    for k in keys_for_cwd {
        let alive = map.get_mut(&k).map(|s| session_is_alive(&mut s.child)).unwrap_or(false);
        if !alive {
            map.remove(&k);
            continue;
        }
        if let Some(s) = map.get(&k) {
            out.push(json!({
                "session_id": s.session_id,
                "port": s.port,
                "alive": true,
                "idle_ms": s.last_used.elapsed().as_millis() as u64,
            }));
        }
    }
    json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "sessions": out})
}

/// `session close <id>` / `session reset <id>` -- explicit kill+remove of a
/// named session for this cwd. `require_found`=true (`close`) errors if the
/// id wasn't present; `require_found`=false (`reset`) is idempotent.
fn session_close(cwd: &Path, target_session_id: &str, require_found: bool) -> Value {
    let key = session_key(cwd, target_session_id);
    let removed = {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&key)
    };
    match removed {
        Some(session) => {
            kill_session(session);
            json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": target_session_id, "closed": true})
        }
        None if require_found => json!({
            "ok": false, "stdout": "", "exit_code": 1,
            "stderr": format!("no live session found for id '{target_session_id}'"),
            "session_id": target_session_id, "closed": false
        }),
        None => json!({"ok": true, "stdout": "", "exit_code": 0, "stderr": "", "session_id": target_session_id, "closed": false}),
    }
}

/// New outermost body-prefix layer, parsed BEFORE strip_mode_prefix/
/// parse_body so it composes cleanly (these commands never carry a script).
enum SessionCommand<'a> {
    New,
    List,
    Close(&'a str),
    Reset(&'a str),
    None,
}

fn parse_session_command(body: &str) -> SessionCommand<'_> {
    let trimmed = body.trim();
    if trimmed == "session new" {
        return SessionCommand::New;
    }
    if trimmed == "session list" {
        return SessionCommand::List;
    }
    if let Some(rest) = trimmed.strip_prefix("session close ") {
        let id = rest.lines().next().unwrap_or("").trim();
        return SessionCommand::Close(id);
    }
    if let Some(rest) = trimmed.strip_prefix("session reset ") {
        let id = rest.lines().next().unwrap_or("").trim();
        return SessionCommand::Reset(id);
    }
    SessionCommand::None
}

/// Extracted launch sequence (find Chrome, pick a free port, spawn with the
/// persistent profile dir, wait for CDP readiness) shared by the session-
/// registry's cache-miss path in `run()` and by `session_new`'s explicit
/// relaunch. Returns the spawned+CDP-ready child and its port, or a
/// human-readable error string.
fn launch_chrome(cwd: &Path, session_id: &str, browser_cfg: &BrowserConfig) -> Result<(Child, u16), String> {
    let chrome = find_chrome().ok_or_else(|| "no Chrome found; install Google Chrome or Chromium".to_string())?;
    let profile_dir = browser_chrome_profile_dir(cwd, session_id);
    let _ = std::fs::create_dir_all(&profile_dir);

    let port = free_port();
    let mut cmd = Command::new(&chrome);
    cmd.arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-default-apps");
    if browser_cfg.headless() {
        cmd.arg("--disable-gpu").arg("--headless=new");
    }
    let mut chrome_child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("chrome launch failed: {e}"))?;

    // Written so a LATER process (this daemon restarted after a crash, or a
    // fresh daemon after a lost-heartbeat exit) can identify this exact OS
    // process as an orphan via `reap_os_orphans`, even though its own
    // in-memory SESSIONS registry has no record of it. Removed on any clean
    // `kill_session`; a leftover file with a still-live pid is precisely the
    // crash-orphan signature the reaper looks for.
    let _ = std::fs::write(pid_sidecar_path(&profile_dir), chrome_child.id().to_string());

    if !cdp_ready(port, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
        let _ = chrome_child.kill();
        let _ = chrome_child.wait();
        let _ = std::fs::remove_file(pid_sidecar_path(&profile_dir));
        return Err(format!(
            "chrome CDP endpoint did not become ready within {}ms",
            browser_cfg.chrome_ready_deadline().as_millis()
        ));
    }
    Ok((chrome_child, port))
}

pub fn run(body: &str, cwd: &Path, session_id: &str) -> Value {
    let Some(node) = which("node") else {
        return json!({"ok": false, "stdout": "", "exit_code": 1,
            "stderr": "node not found on PATH; required to drive Chrome over CDP"});
    };

    let t0 = Instant::now();
    let browser_cfg = BrowserConfig::load(cwd);
    reap_idle_sessions(cwd, &browser_cfg);
    reap_os_orphans(cwd);

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

    // New outermost prefix layer: session-management commands short-circuit
    // straight to their own JSON envelope, never running a script or
    // touching strip_mode_prefix/parse_body.
    match parse_session_command(&inner_body) {
        SessionCommand::New => return session_new(cwd, session_id, &browser_cfg),
        SessionCommand::List => return session_list(cwd),
        SessionCommand::Close(id) if !id.is_empty() => return session_close(cwd, id, true),
        SessionCommand::Reset(id) if !id.is_empty() => return session_close(cwd, id, false),
        SessionCommand::Close(_) | SessionCommand::Reset(_) => {
            return json!({"ok": false, "stdout": "", "exit_code": 1,
                "stderr": "session close/reset requires an explicit id, e.g. 'session close default'"});
        }
        SessionCommand::None => {}
    }

    // Mode (capture/profile/trace) and timeout= are both outer stackable
    // prefixes that must compose with EACH OTHER in either order, and with
    // the url=/bare-URL prefix parse_body handles next -- loop stripping
    // whichever matches until neither does, rather than the old fixed
    // single-shot mode-then-url order that silently swallowed a stacked
    // `timeout=` line whole (see strip_timeout_prefix's doc comment for the
    // exact syntax-error mechanism this fixes).
    let mut mode = BrowserMode::Default;
    let mut timeout_override: Option<u64> = None;
    let mut rest: &str = &inner_body;
    loop {
        let (m, after_mode) = strip_mode_prefix(rest);
        if m != BrowserMode::Default {
            mode = m;
            rest = after_mode;
            continue;
        }
        let (t, after_timeout) = strip_timeout_prefix(rest);
        if let Some(ms) = t {
            timeout_override = Some(ms);
            rest = after_timeout;
            continue;
        }
        break;
    }
    let (start_url, script) = parse_body(rest);
    let timeout_ms = timeout_override.unwrap_or(timeout_ms);

    let tmp = std::env::temp_dir();
    let stamp = format!("{}-{}", std::process::id(), sanitize(session_id));
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

    // Reuse a live session's Chrome + port if one exists for this (cwd,
    // session_id) key; otherwise launch fresh and register it BEFORE
    // evaluating the script, so a script that itself crashes/hangs never
    // leaves the registry inconsistent.
    let key = session_key(cwd, session_id);
    let port = {
        let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
        let reuse_port = map.get_mut(&key).and_then(|s| {
            if session_is_alive(&mut s.child) {
                s.last_used = Instant::now();
                Some(s.port)
            } else {
                None
            }
        });
        if reuse_port.is_none() {
            map.remove(&key);
        }
        reuse_port
    };
    let port = match port {
        Some(p) => p,
        None => {
            let (chrome_child, new_port) = match launch_chrome(cwd, session_id, &browser_cfg) {
                Ok(v) => v,
                Err(e) => {
                    cleanup(&[&helper_path, &script_path, &result_path]);
                    return json!({"ok": false, "stdout": "", "exit_code": 1, "stderr": e});
                }
            };
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                key.clone(),
                BrowserSession {
                    cwd: cwd.to_path_buf(),
                    session_id: session_id.to_string(),
                    child: chrome_child,
                    port: new_port,
                    last_used: Instant::now(),
                },
            );
            new_port
        }
    };

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
            // The session's Chrome is a registry-owned, cross-dispatch
            // resource now -- a node-helper spawn failure is this
            // dispatch's own problem, never a reason to tear down a
            // process other dispatches under the same session_id still
            // expect to find alive.
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

    // Live-found (headless-chrome-elimination investigation): a timeout here
    // previously killed ONLY the transient node cdp-eval helper -- the
    // actual Chrome process stayed registered in SESSIONS and got reused on
    // the very next dispatch even when the timeout's real cause was a wedged
    // page/renderer (not a slow-but-fine one), because a hung Chrome tab
    // still answers a top-level CDP `/json/version` HTTP probe (that's
    // Chrome's own always-alive browser process, not the wedged renderer) --
    // so a single re-poll here can't distinguish "helper was just slow" from
    // "renderer is wedged." The escalation this fixes: an eval that timed
    // out gets ONE bounded re-check against the SAME page context it was
    // just driving (a trivial `1+1` Runtime.evaluate over the session's own
    // CDP port, not merely the version endpoint) with a short deadline; if
    // that also fails to answer, the renderer is genuinely wedged (not just
    // last-eval-slow) and the session is killed + evicted from SESSIONS so
    // the next dispatch launches a fresh Chrome instead of reusing a corpse
    // that will time out on every future eval too. A responsive re-check
    // means the timeout really was this one script (e.g. an intentional
    // `await new Promise(()=>{})`), so the Chrome process is left alone and
    // reused normally.
    if timed_out && !session_liveness_recheck(port, &browser_cfg) {
        eprintln!(
            "[agentplug browser] eval timeout AND page unresponsive to a follow-up probe -- session '{}' (port {}) is wedged, killing and evicting so the next dispatch gets a fresh Chrome",
            session_id, port
        );
        let dead = {
            let mut map = sessions_map().lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&key)
        };
        if let Some(session) = dead {
            kill_session(session);
        }
    }

    let mut stderr_buf = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = std::io::Read::read_to_end(&mut err, &mut stderr_buf);
    }
    let exit_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);

    let result_value: Value = std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);

    cleanup(&[&helper_path, &script_path, &result_path]);
    // Neither the session's Chrome process nor its persistent profile dir
    // (.gm/browser-chrome-profile-<session_id>/) is torn down here anymore --
    // the process now lives in the SESSIONS registry and survives across
    // dispatches under the same session_id until an explicit `session
    // close`/`session reset`, the idle-timeout reaper, or daemon exit kills
    // it (see the SESSIONS registry doc comments above).

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 1 regression test: on macOS, find_chrome() must locate the
    /// standard app-bundle install even with no `chrome`/`google-chrome`
    /// on PATH. This runs the real function against the real filesystem
    /// (no mocking) -- on a macOS CI/dev box with Chrome installed at the
    /// standard location, this previously returned None and every browser
    /// dispatch failed with "no Chrome found".
    #[test]
    #[cfg(target_os = "macos")]
    fn find_chrome_locates_macos_app_bundle() {
        let found = find_chrome();
        assert!(
            found.is_some(),
            "find_chrome() should locate a macOS Chrome/Chromium install; got None"
        );
        let path = found.unwrap();
        assert!(path.exists(), "resolved chrome path {:?} does not exist", path);
    }

    /// Bug 2 regression test: stacking `timeout=<ms>` in front of `url=`
    /// must extract BOTH correctly, not swallow the url= line into the
    /// script. This is the exact repro from the live-hit bug report.
    #[test]
    fn timeout_then_url_prefix_stack_parses_correctly() {
        let body = "timeout=30000\nurl=http://x/\nreturn 1+1;";
        let mut mode = BrowserMode::Default;
        let mut timeout_override: Option<u64> = None;
        let mut rest: &str = body;
        loop {
            let (m, after_mode) = strip_mode_prefix(rest);
            if m != BrowserMode::Default {
                mode = m;
                rest = after_mode;
                continue;
            }
            let (t, after_timeout) = strip_timeout_prefix(rest);
            if let Some(ms) = t {
                timeout_override = Some(ms);
                rest = after_timeout;
                continue;
            }
            break;
        }
        let (start_url, script) = parse_body(rest);

        assert_eq!(mode, BrowserMode::Default);
        assert_eq!(timeout_override, Some(30000));
        assert_eq!(start_url, Some("http://x/".to_string()));
        assert_eq!(script, "return 1+1;");
    }

    /// The single-prefix case (`url=` alone, no `timeout=`) must keep
    /// working byte-identically to before this fix.
    #[test]
    fn url_prefix_alone_still_works() {
        let body = "url=http://x/\nreturn 1+1;";
        let (mode, after_mode) = strip_mode_prefix(body);
        assert_eq!(mode, BrowserMode::Default);
        let (t, after_timeout) = strip_timeout_prefix(after_mode);
        assert_eq!(t, None);
        let (start_url, script) = parse_body(after_timeout);
        assert_eq!(start_url, Some("http://x/".to_string()));
        assert_eq!(script, "return 1+1;");
    }

    /// mode + timeout + url all stacked, mode first: composition must be
    /// fully order-independent between the two outer prefixes.
    #[test]
    fn mode_then_timeout_then_url_all_compose() {
        let body = "capture\ntimeout=5000\nurl=http://x/\nreturn 42;";
        let mut mode = BrowserMode::Default;
        let mut timeout_override: Option<u64> = None;
        let mut rest: &str = body;
        loop {
            let (m, after_mode) = strip_mode_prefix(rest);
            if m != BrowserMode::Default {
                mode = m;
                rest = after_mode;
                continue;
            }
            let (t, after_timeout) = strip_timeout_prefix(rest);
            if let Some(ms) = t {
                timeout_override = Some(ms);
                rest = after_timeout;
                continue;
            }
            break;
        }
        let (start_url, script) = parse_body(rest);

        assert_eq!(mode, BrowserMode::Capture);
        assert_eq!(timeout_override, Some(5000));
        assert_eq!(start_url, Some("http://x/".to_string()));
        assert_eq!(script, "return 42;");
    }

    /// timeout + mode stacked, timeout first: same as above but reversed
    /// order, proving the loop (not a fixed mode-then-timeout sequence)
    /// is what makes this compose.
    #[test]
    fn timeout_then_mode_then_url_all_compose() {
        let body = "timeout=5000\ncapture\nurl=http://x/\nreturn 42;";
        let mut mode = BrowserMode::Default;
        let mut timeout_override: Option<u64> = None;
        let mut rest: &str = body;
        loop {
            let (m, after_mode) = strip_mode_prefix(rest);
            if m != BrowserMode::Default {
                mode = m;
                rest = after_mode;
                continue;
            }
            let (t, after_timeout) = strip_timeout_prefix(rest);
            if let Some(ms) = t {
                timeout_override = Some(ms);
                rest = after_timeout;
                continue;
            }
            break;
        }
        let (start_url, script) = parse_body(rest);

        assert_eq!(mode, BrowserMode::Capture);
        assert_eq!(timeout_override, Some(5000));
        assert_eq!(start_url, Some("http://x/".to_string()));
        assert_eq!(script, "return 42;");
    }

    /// End-to-end live verification through the REAL public entry point,
    /// driving a REAL Chrome process over CDP: the exact stacked-prefix
    /// dispatch body that used to fail instantly with `SyntaxError:
    /// Unexpected token ':'` must now navigate and evaluate correctly.
    /// Requires Chrome + node on the host, same as any real dispatch;
    /// skips itself (rather than failing) if either is absent so this
    /// doesn't break CI runners without a browser.
    #[test]
    fn run_end_to_end_timeout_and_url_stack() {
        if which("node").is_none() {
            eprintln!("skipping run_end_to_end_timeout_and_url_stack: node not on PATH");
            return;
        }
        if find_chrome().is_none() {
            eprintln!("skipping run_end_to_end_timeout_and_url_stack: no Chrome found");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("agentplug-browser-test-{}", unix_ms()));
        std::fs::create_dir_all(tmp.join(".gm")).unwrap();
        // Force headless so this doesn't pop a visible window in CI/dev.
        std::fs::write(
            tmp.join(".gm").join("browser-config.json"),
            r#"{"headless": true}"#,
        )
        .unwrap();

        let envelope = json!({
            "body": "timeout=30000\nurl=data:text/plain,hello\nreturn 1+1;",
            "timeoutMs": 3000,
        })
        .to_string();

        let result = run(&envelope, &tmp, "test-timeout-url-stack");

        // Close whatever Chrome session this test launched so it doesn't
        // linger as an orphaned process after the test process exits.
        let _ = session_close(&tmp, "test-timeout-url-stack", true);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "expected ok:true, got {result:?}"
        );
        assert_eq!(
            result.get("result").and_then(|v| v.as_i64()),
            Some(2),
            "expected result:2 (1+1), got {result:?}"
        );
    }
}
