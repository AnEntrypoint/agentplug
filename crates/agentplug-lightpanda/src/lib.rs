//! Headless crawl engine: one warm `lightpanda serve` per project root.
//!
//! The process is started on the first crawl, kept across dispatches, and
//! reaped after GM_LIGHTPANDA_IDLE_SECONDS (default 300) without a crawl. The
//! CDP target id is remembered so repeat crawls reattach to the same page
//! instead of creating a new one. A Linux server carries PR_SET_PDEATHSIG and
//! dies with the runner; macOS has no pdeathsig, so there the idle reap and
//! `shutdown` are the only cleanup of a warm server whose runner was killed.
//! PR_SET_PDEATHSIG fires when the spawning thread exits, so servers are spawned
//! on one owner thread that lives as long as the runner (`spawn_owned`).
//!
//! Binary order: GM_LIGHTPANDA_PATH, then `<agentplug home>/bin/lightpanda-<arch>-<os>`
//! (the lightpanda-io/browser release asset names), then `lightpanda` on PATH.
//! lightpanda has no native Windows build; on Windows the server runs inside WSL2
//! through `wsl.exe --exec`, the path the upstream README documents.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use agentplug_host::{
    apply_windowless, canonical_project_root, crawl_error_reply, crawl_reply_from_run,
    endpoint_ready, find_on_path, free_local_port, install_dir, parse_crawl_body, run_helper,
};
use serde_json::{json, Value};

const ENGINE: &str = "lightpanda";
const DEFAULT_IDLE_SECONDS: u64 = 300;
const REAPER_TICK: Duration = Duration::from_secs(5);
const READY_DEADLINE: Duration = Duration::from_secs(15);
const WSL_READY_DEADLINE: Duration = Duration::from_secs(45);
const READY_POLL: Duration = Duration::from_millis(100);
const HELPER_BUDGET: Duration = Duration::from_secs(300);

enum Launch {
    Native(PathBuf),
    Wsl {
        wsl: PathBuf,
        distro: Option<String>,
        binary: String,
    },
}

struct Warm {
    child: Child,
    port: u16,
    launch: Launch,
    target_id: Option<String>,
    last_used: Instant,
    busy: bool,
}

static WARM: OnceLock<Mutex<HashMap<PathBuf, Warm>>> = OnceLock::new();
static REAPER: OnceLock<()> = OnceLock::new();
static SPAWNER: OnceLock<mpsc::Sender<SpawnJob>> = OnceLock::new();

type SpawnJob = (Command, mpsc::Sender<io::Result<Child>>);

fn spawn_owned(cmd: Command) -> io::Result<Child> {
    let sender = SPAWNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SpawnJob>();
        std::thread::spawn(move || {
            for (mut cmd, reply) in rx {
                let _ = reply.send(cmd.spawn());
            }
        });
        tx
    });
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .send((cmd, reply_tx))
        .map_err(|_| io::Error::other("lightpanda spawner thread is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| io::Error::other("lightpanda spawner thread dropped the spawn reply"))?
}

#[cfg(target_os = "linux")]
fn die_with_runner(cmd: &mut Command, runner_pid: u32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe libc functions (prctl, getppid, raise) on scalar values.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != runner_pid {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

fn warm_map() -> MutexGuard<'static, HashMap<PathBuf, Warm>> {
    WARM.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn idle_ttl() -> Duration {
    let seconds = std::env::var("GM_LIGHTPANDA_IDLE_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_IDLE_SECONDS);
    Duration::from_secs(seconds)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn platform_asset_name() -> Option<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        _ => return None,
    };
    Some(format!("lightpanda-{}-{os}", std::env::consts::ARCH))
}

fn resolve_native_binary() -> Result<PathBuf, String> {
    if let Some(raw) = env_value("GM_LIGHTPANDA_PATH") {
        let path = PathBuf::from(raw);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "GM_LIGHTPANDA_PATH names {}, which is not a file",
                path.display()
            ))
        };
    }
    let installed = platform_asset_name().map(|name| install_dir().join("bin").join(name));
    if let Some(path) = installed.as_ref().filter(|p| p.is_file()) {
        return Ok(path.clone());
    }
    find_on_path("lightpanda").ok_or_else(|| {
        let expected = installed
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("(no release asset for {})", std::env::consts::OS));
        format!(
            "lightpanda binary not found: set GM_LIGHTPANDA_PATH, install the official lightpanda-io/browser release asset at {expected}, or put lightpanda on PATH; nothing is downloaded"
        )
    })
}

fn wsl_launch() -> Result<Launch, String> {
    let wsl = find_on_path("wsl").ok_or_else(|| {
        "lightpanda has no native Windows build: the documented path is WSL2 -- run `wsl --install`, install the Linux lightpanda release inside WSL, and make it reachable as GM_LIGHTPANDA_WSL_BINARY (default `lightpanda` on the WSL PATH); wsl.exe was not found".to_string()
    })?;
    Ok(Launch::Wsl {
        wsl,
        distro: env_value("GM_LIGHTPANDA_WSL_DISTRO"),
        binary: env_value("GM_LIGHTPANDA_WSL_BINARY").unwrap_or_else(|| "lightpanda".to_string()),
    })
}

fn describe(launch: &Launch) -> String {
    match launch {
        Launch::Native(binary) => binary.display().to_string(),
        Launch::Wsl { binary, .. } => format!("wsl.exe --exec {binary}"),
    }
}

fn serve_command(launch: &Launch, port: u16) -> Command {
    let port_arg = port.to_string();
    match launch {
        Launch::Native(binary) => {
            let mut cmd = Command::new(binary);
            cmd.args(["serve", "--host", "127.0.0.1", "--port", port_arg.as_str()]);
            #[cfg(target_os = "linux")]
            die_with_runner(&mut cmd, std::process::id());
            cmd
        }
        Launch::Wsl {
            wsl,
            distro,
            binary,
        } => {
            let mut cmd = Command::new(wsl);
            if let Some(distro) = distro {
                cmd.args(["--distribution", distro.as_str()]);
            }
            cmd.args([
                "--exec",
                binary.as_str(),
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                port_arg.as_str(),
            ]);
            cmd
        }
    }
}

fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let last: Vec<&str> = text.lines().rev().take(5).collect();
    let tail = last.into_iter().rev().collect::<Vec<_>>().join(" | ");
    if tail.is_empty() {
        "lightpanda produced no output".to_string()
    } else {
        tail
    }
}

fn start_warm(key: &Path) -> Result<Warm, String> {
    let launch = if cfg!(windows) {
        wsl_launch()?
    } else {
        Launch::Native(resolve_native_binary()?)
    };
    let port = free_local_port()?;
    let dir = key.join(".gm").join("lightpanda");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let log_path = dir.join("serve.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("could not open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("could not clone the lightpanda log handle: {e}"))?;
    let mut cmd = serve_command(&launch, port);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    apply_windowless(&mut cmd);
    let child = spawn_owned(cmd)
        .map_err(|e| format!("lightpanda launch failed ({}): {e}", describe(&launch)))?;
    let mut warm = Warm {
        child,
        port,
        launch,
        target_id: None,
        last_used: Instant::now(),
        busy: false,
    };
    let ready_deadline = if cfg!(windows) {
        WSL_READY_DEADLINE
    } else {
        READY_DEADLINE
    };
    if !endpoint_ready(port, Instant::now() + ready_deadline, READY_POLL) {
        let tail = log_tail(&log_path);
        stop(&mut warm);
        return Err(format!(
            "lightpanda CDP endpoint on port {port} did not become ready within {}ms (recent output: {tail})",
            ready_deadline.as_millis()
        ));
    }
    Ok(warm)
}

/// Kills the serve process. Under WSL the Linux-side server is a separate
/// process that wsl.exe does not own, so it is also matched and killed inside
/// the distribution.
fn stop(warm: &mut Warm) {
    let _ = warm.child.kill();
    let _ = warm.child.wait();
    if let Launch::Wsl {
        wsl,
        distro,
        binary,
    } = &warm.launch
    {
        let pattern = format!("{binary} serve --host 127.0.0.1 --port {}", warm.port);
        let mut cmd = Command::new(wsl);
        if let Some(distro) = distro {
            cmd.args(["--distribution", distro.as_str()]);
        }
        cmd.args(["--exec", "pkill", "-f", pattern.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_windowless(&mut cmd);
        let _ = cmd.status();
    }
}

/// Returns the warm port and remembered target for `key`, starting the process
/// when none is alive. Marks the entry busy until `release`.
fn acquire(key: &Path) -> Result<(u16, Option<String>), String> {
    let mut map = warm_map();
    if let Some(warm) = map.get_mut(key) {
        if warm.busy {
            return Err(
                "lightpanda is already running a crawl for this project; retry when it finishes"
                    .to_string(),
            );
        }
        if matches!(warm.child.try_wait(), Ok(None)) {
            warm.busy = true;
            return Ok((warm.port, warm.target_id.clone()));
        }
    }
    if let Some(mut stale) = map.remove(key) {
        stop(&mut stale);
    }
    let mut warm = start_warm(key)?;
    warm.busy = true;
    let port = warm.port;
    map.insert(key.to_path_buf(), warm);
    start_reaper();
    Ok((port, None))
}

fn release(key: &Path, target_id: Option<String>) {
    if let Some(warm) = warm_map().get_mut(key) {
        warm.busy = false;
        warm.last_used = Instant::now();
        if target_id.is_some() {
            warm.target_id = target_id;
        }
    }
}

fn start_reaper() {
    if REAPER.set(()).is_err() {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(REAPER_TICK);
        let ttl = idle_ttl();
        let mut map = warm_map();
        let expired: Vec<PathBuf> = map
            .iter_mut()
            .filter(|(_, warm)| !warm.busy)
            .filter_map(|(key, warm)| {
                let idle = warm.last_used.elapsed() >= ttl;
                let exited = !matches!(warm.child.try_wait(), Ok(None));
                (idle || exited).then(|| key.clone())
            })
            .collect();
        for key in expired {
            if let Some(mut warm) = map.remove(&key) {
                stop(&mut warm);
            }
        }
    });
}

/// Runs a crawl body on the warm lightpanda for `cwd`'s project root. The
/// body may start with `engine=lightpanda`; `engine=cdp` is refused here.
pub fn crawl(cwd: &Path, body: &str) -> Value {
    let started = Instant::now();
    let parsed = match parse_crawl_body(body) {
        Ok(parsed) => parsed,
        Err(e) => return crawl_error_reply(ENGINE, true, started, e),
    };
    if matches!(parsed.engine.as_deref(), Some(other) if other != ENGINE) {
        return crawl_error_reply(
            ENGINE,
            true,
            started,
            "engine=cdp is served by crawl_cdp in agentplug-host, not the lightpanda plugin"
                .to_string(),
        );
    }
    let key = canonical_project_root(cwd);
    let (port, target_id) = match acquire(&key) {
        Ok(acquired) => acquired,
        Err(e) => return crawl_error_reply(ENGINE, true, started, e),
    };
    let run = run_helper(port, target_id.as_deref(), &parsed.steps, HELPER_BUDGET, true);
    let remembered = run
        .as_ref()
        .ok()
        .and_then(|r| r.result.as_ref())
        .and_then(|v| v.get("targetId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    release(&key, remembered.or(target_id));
    match run {
        Ok(run) => crawl_reply_from_run(ENGINE, true, started, run),
        Err(e) => crawl_error_reply(ENGINE, true, started, e),
    }
}

/// Plugin verb dispatch. `crawl` takes a crawl body; any other verb answers
/// unknown_verb.
pub fn plugin_call(cwd: &Path, verb: &str, body: &str) -> Value {
    match verb {
        "crawl" => crawl(cwd, body),
        _ => json!({"ok": false, "error": "unknown_verb", "verb": verb}),
    }
}

/// Kills every warm lightpanda process. Call at runner shutdown.
pub fn shutdown() {
    for (_, mut warm) in warm_map().drain() {
        stop(&mut warm);
    }
}
