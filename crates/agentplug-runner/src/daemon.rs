use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wasmtime::{Engine, Module};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, DispatchHandle, GmFairnessGuard, ProjectPlugins, PLUGIN_IDLE_EVICT_MS};

use crate::download::{ensure_plugin_installed, installed_runner_version, record_runner_version};

fn registry_path() -> PathBuf {
    install_dir().join("daemon-registry.txt")
}

pub fn register_project(cwd: &Path) -> anyhow::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let cwd_str = cwd.to_string_lossy().to_string();
    if existing.lines().any(|l| l.trim() == cwd_str) {
        return Ok(());
    }
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{cwd_str}")?;
    Ok(())
}

fn read_registry() -> Vec<PathBuf> {
    fs::read_to_string(registry_path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

#[derive(serde::Deserialize, Clone, Copy)]
struct DaemonConfig {
    #[serde(default)]
    registry_poll_interval_secs: Option<u64>,
    #[serde(default)]
    heartbeat_interval_secs: Option<u64>,
    #[serde(default)]
    plugin_update_poll_interval_secs: Option<u64>,
    #[serde(default)]
    runner_update_poll_interval_secs: Option<u64>,
    #[serde(default)]
    max_concurrent_projects: Option<usize>,
    #[serde(default)]
    gm_concurrency: Option<usize>,
    #[serde(default)]
    side_plugin_concurrency: Option<usize>,
}

const DAEMON_CONFIG_EXAMPLE: &str = r#"{
  "registry_poll_interval_secs": 5,
  "heartbeat_interval_secs": 10,
  "plugin_update_poll_interval_secs": 600,
  "runner_update_poll_interval_secs": 600,
  "max_concurrent_projects": 4,
  "gm_concurrency": 4,
  "side_plugin_concurrency": 1
}
"#;

impl DaemonConfig {
    fn scaffold_example_if_absent() {
        let path = install_dir().join("daemon-config.json");
        if path.exists() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, DAEMON_CONFIG_EXAMPLE);
    }

    fn load() -> Self {
        Self::scaffold_example_if_absent();
        let path = install_dir().join("daemon-config.json");
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<DaemonConfig>(&s).ok())
            .unwrap_or(DaemonConfig {
                registry_poll_interval_secs: None,
                heartbeat_interval_secs: None,
                plugin_update_poll_interval_secs: None,
                runner_update_poll_interval_secs: None,
                max_concurrent_projects: None,
                gm_concurrency: None,
                side_plugin_concurrency: None,
            })
    }
    fn registry_poll_interval(&self) -> Duration { Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5)) }
    fn heartbeat_interval(&self) -> Duration { Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10)) }
    fn plugin_update_poll_interval(&self) -> Duration { Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600)) }
    fn runner_update_poll_interval(&self) -> Duration { Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(600)) }
    fn max_concurrent_projects(&self) -> usize { self.max_concurrent_projects.unwrap_or(4).max(1) }
    fn gm_concurrency(&self) -> usize { self.gm_concurrency.unwrap_or_else(|| self.max_concurrent_projects()).max(1) }
    fn side_plugin_concurrency(&self) -> usize { self.side_plugin_concurrency.unwrap_or(1).max(1) }
}

const DAEMON_STALE_MS: u64 = 20_000;

fn daemon_status_path() -> PathBuf {
    install_dir().join("daemon-status.json")
}

fn daemon_lock_path() -> PathBuf {
    install_dir().join("daemon.lock")
}

fn daemon_owner_path() -> PathBuf {
    install_dir().join("daemon-owner.lock")
}

fn read_owner_pid() -> Option<u64> {
    fs::read_to_string(daemon_owner_path()).ok().and_then(|s| s.trim().parse::<u64>().ok())
}

pub fn claim_ownership() -> bool {
    let owner_path = daemon_owner_path();
    if let Some(parent) = owner_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let my_pid = std::process::id() as u64;

    if fs::OpenOptions::new().write(true).create_new(true).open(&owner_path).is_ok() {
        use std::io::Write as _;
        if let Ok(mut f) = fs::OpenOptions::new().write(true).open(&owner_path) {
            let _ = write!(f, "{my_pid}");
        }
        return true;
    }

    let existing_pid = read_owner_pid();
    let heartbeat_fresh = fs::read_to_string(daemon_status_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|v| {
            let pid = v.get("pid").and_then(|p| p.as_u64());
            let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
            now_ms().saturating_sub(ts) < DAEMON_STALE_MS && pid == existing_pid
        })
        .unwrap_or(false);
    if heartbeat_fresh && existing_pid.map(pid_is_alive).unwrap_or(false) {
        return existing_pid == Some(my_pid);
    }

    let tmp_path = owner_path.with_extension(format!("lock.tmp.{my_pid}"));
    if fs::write(&tmp_path, my_pid.to_string()).is_err() {
        return false;
    }
    if fs::rename(&tmp_path, &owner_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return false;
    }
    read_owner_pid() == Some(my_pid)
}

fn holds_heartbeat_authority() -> bool {
    match read_owner_pid() {
        None => claim_ownership(),
        Some(pid) if pid == std::process::id() as u64 => true,
        Some(_) => claim_ownership() && read_owner_pid() == Some(std::process::id() as u64),
    }
}

pub fn ensure_daemon_running() -> anyhow::Result<bool> {
    if is_daemon_fresh() {
        return Ok(true);
    }
    let lock_path = daemon_lock_path();
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let acquired = fs::OpenOptions::new().write(true).create_new(true).open(&lock_path).is_ok();
    if !acquired {
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(200));
            if is_daemon_fresh() {
                return Ok(true);
            }
        }
        let _ = fs::remove_file(&lock_path);
        return Ok(false);
    }
    let spawn_result = spawn_detached_daemon();
    let _ = fs::remove_file(&lock_path);
    spawn_result?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(200));
        if is_daemon_fresh() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_daemon_fresh() -> bool {
    let Ok(raw) = fs::read_to_string(daemon_status_path()) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    let Some(ts) = v.get("ts").and_then(|t| t.as_u64()) else { return false };
    if now_ms().saturating_sub(ts) >= DAEMON_STALE_MS { return false; }
    let Some(pid) = v.get("pid").and_then(|p| p.as_u64()) else { return false };
    pid_is_alive(pid)
}

#[cfg(windows)]
fn pid_is_alive(pid: u64) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

fn spawn_detached(exe: &Path, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

fn spawn_detached_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    spawn_detached(&exe, &["daemon"])
}

fn takeover_ready_path() -> PathBuf {
    install_dir().join("daemon-takeover-ready.json")
}

#[derive(serde::Deserialize)]
struct InstructionSourceConfig {
    repo: String,
    #[serde(default = "default_branch")]
    branch: String,
    #[allow(dead_code)]
    #[serde(default)]
    path: String,
}
fn default_branch() -> String { "main".to_string() }

fn instruction_source_config_path(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions").join("source.json")
}

fn instruction_source_cache_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions-source-cache")
}

fn sync_instruction_source_if_configured(root: &Path) -> anyhow::Result<()> {
    let config_path = instruction_source_config_path(root);
    let Ok(raw) = fs::read_to_string(&config_path) else { return Ok(()) };
    let Ok(cfg) = serde_json::from_str::<InstructionSourceConfig>(&raw) else {
        eprintln!("[agentplug daemon] {} exists but does not parse as {{repo, branch?, path?}} -- ignoring", config_path.display());
        return Ok(());
    };
    let cache_dir = instruction_source_cache_dir(root);
    let git_dir_marker = cache_dir.join(".git");
    if !git_dir_marker.exists() {
        fs::create_dir_all(root.join(".gm"))?;
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", &cfg.branch, &cfg.repo, &cache_dir.to_string_lossy()])
            .status()?;
        if !status.success() {
            anyhow::bail!("git clone of {} (branch {}) failed", cfg.repo, cfg.branch);
        }
        eprintln!("[agentplug daemon] cloned instruction source {} (branch {}) for {}", cfg.repo, cfg.branch, root.display());
        return Ok(());
    }
    let fetch = std::process::Command::new("git")
        .args(["-C", &cache_dir.to_string_lossy(), "fetch", "--depth", "1", "origin", &cfg.branch])
        .status()?;
    if !fetch.success() {
        anyhow::bail!("git fetch of {} (branch {}) failed", cfg.repo, cfg.branch);
    }
    let reset = std::process::Command::new("git")
        .args(["-C", &cache_dir.to_string_lossy(), "reset", "--hard", &format!("origin/{}", cfg.branch)])
        .status()?;
    if !reset.success() {
        anyhow::bail!("git reset of instruction source cache for {} failed", root.display());
    }
    Ok(())
}

fn attempt_self_update_handoff(staged_exe: &Path, version: &str) -> bool {
    let ready_path = takeover_ready_path();
    let _ = fs::remove_file(&ready_path);
    if spawn_detached(staged_exe, &["takeover", version]).is_err() {
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(raw) = fs::read_to_string(&ready_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v.get("version").and_then(|x| x.as_str()) == Some(version) {
                    eprintln!("[agentplug daemon] new version {version} confirmed ready -- releasing ownership for handoff");
                    release_ownership_for_handoff();
                    return true;
                }
            }
        }
    }
    eprintln!("[agentplug daemon] self-update to {version} did not confirm ready in time -- staying on current version, will retry next poll");
    false
}

fn release_ownership_for_handoff() {
    let my_pid = std::process::id() as u64;
    if read_owner_pid() == Some(my_pid) {
        let _ = fs::remove_file(daemon_owner_path());
    }
}

pub fn run_takeover(version: &str) -> anyhow::Result<()> {
    eprintln!("[agentplug daemon] takeover: building engine for version {version}");
    let mut plugin_modules = PluginModules::new()?;
    for plugin_name in ["gm", "bert", "libsql", "treesitter"] {
        if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
            eprintln!("[agentplug daemon] takeover: pre-warm of {plugin_name} failed (non-fatal, will lazy-compile on first use): {e}");
        }
    }
    let _ = fs::write(
        takeover_ready_path(),
        serde_json::json!({"version": version, "pid": std::process::id(), "ts": now_ms()}).to_string(),
    );
    eprintln!("[agentplug daemon] takeover: readiness marker written, waiting for old daemon to release ownership");
    for _ in 0..480 {
        if read_owner_pid().is_none() && claim_ownership() {
            record_runner_version(version)?;
            eprintln!("[agentplug daemon] takeover: ownership claimed, version recorded, entering normal daemon loop");
            return run_daemon_body(plugin_modules);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("takeover: old daemon never released ownership within the wait window -- aborting, old daemon keeps serving")
}

fn write_daemon_heartbeat(project_count: usize, plugin_module_count: usize) {
    let _ = fs::write(
        daemon_status_path(),
        serde_json::json!({
            "pid": std::process::id(),
            "ts": now_ms(),
            "active_projects": project_count,
            "compiled_plugin_modules": plugin_module_count,
        })
        .to_string(),
    );
}

static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static HEARTBEAT_AUTHORITY_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn heartbeat_authority_lost() -> bool {
    HEARTBEAT_AUTHORITY_LOST.load(std::sync::atomic::Ordering::Relaxed)
}

fn spawn_heartbeat_ticker(heartbeat_interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        std::thread::sleep(heartbeat_interval);
        if heartbeat_authority_lost() {
            return;
        }
        if !holds_heartbeat_authority() {
            eprintln!("[agentplug daemon] heartbeat ticker: authority lost to another daemon -- signaling main loop to exit");
            HEARTBEAT_AUTHORITY_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        write_daemon_heartbeat(
            HEARTBEAT_PROJECT_COUNT.load(std::sync::atomic::Ordering::Relaxed),
            HEARTBEAT_PLUGIN_MODULE_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        );
    })
}

struct PluginModules {
    engine: Engine,
    modules: HashMap<String, Module>,
}

impl PluginModules {
    fn new() -> anyhow::Result<Self> {
        Ok(Self { engine: build_engine()?, modules: HashMap::new() })
    }

    fn get_or_compile(&mut self, plugin_name: &str) -> anyhow::Result<()> {
        if !self.modules.contains_key(plugin_name) {
            let wasm_path = ensure_plugin_installed(plugin_name, None)?;
            eprintln!("[agentplug daemon] compiling {plugin_name}.wasm (shared across every project that uses it)...");
            let module = Module::from_file(&self.engine, &wasm_path)?;
            self.modules.insert(plugin_name.to_string(), module);
        }
        Ok(())
    }
}

type InFlightKey = (PathBuf, String, String);

struct InFlightHandle {
    detach: Arc<std::sync::atomic::AtomicBool>,
}

static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn handle_background_convert(root: &Path, body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Req {
        verb: String,
        task: String,
    }
    let req: Req = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return serde_json::json!({"ok": false, "error": format!("background-convert body must be {{verb, task}}: {e}")}).to_string();
        }
    };
    let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
    let mut map = in_flight_map().lock().unwrap_or_else(|e| e.into_inner());
    match map.remove(&key) {
        Some(handle) => {
            handle.detach.store(true, std::sync::atomic::Ordering::SeqCst);
            serde_json::json!({"ok": true, "converted": true, "verb": req.verb, "task": req.task}).to_string()
        }
        None => {
            serde_json::json!({"ok": false, "error": "already_completed", "verb": req.verb, "task": req.task}).to_string()
        }
    }
}

fn run_gm_dispatch_to_file(root: &Path, handle: &DispatchHandle, verb: &str, task: &str, body: &str, out_dir: &Path) {
    let _fairness_guard = GmFairnessGuard::acquire(root);
    let dispatch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch("gm", verb, body)));
    let out_body = match dispatch_result {
        Ok(Ok(s)) if !s.is_empty() => s,
        Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
        Ok(Err(e)) => serde_json::json!({"ok": false, "error": format!("{e:#}"), "verb": verb}).to_string(),
        Err(panic_payload) => {
            let msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic with non-string payload".to_string());
            eprintln!("[agentplug daemon] verb {verb} PANICKED for {}: {msg}", root.display());
            serde_json::json!({"ok": false, "error": format!("dispatch panicked: {msg}"), "verb": verb}).to_string()
        }
    };
    let out_name = format!("{verb}-{task}.json");
    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
    if fs::write(&tmp, &out_body).is_ok() {
        let _ = fs::rename(&tmp, out_dir.join(&out_name));
        let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
    }
}

fn dispatch_project(root: &Path, project: &mut ProjectPlugins, plugin_modules: &PluginModules) -> bool {
    let mut did_work = false;

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }

    let status_path = spool_dir.join(".status.json");
    let _ = fs::write(
        &status_path,
        serde_json::json!({"pid": std::process::id(), "ts": now_ms(), "daemon": true, "shared_process": true, "runtime": "agentplug"}).to_string(),
    );

    let requested_plugins = {
        let mut list = read_project_plugin_list(root);
        if list.is_empty() {
            list.push("gm".to_string());
            list.push("libsql".to_string());
            list.push("bert".to_string());
            list.push("treesitter".to_string());
        }
        list
    };

    struct ClaimedRequest {
        verb: String,
        task: String,
        body: String,
    }
    let mut claimed: Vec<ClaimedRequest> = Vec::new();
    if let Ok(entries) = fs::read_dir(&in_dir) {
        for verb_entry in entries.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let verb_dir = verb_entry.path();
            let Ok(files) = fs::read_dir(&verb_dir) else { continue };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                    continue;
                }
                let claim_path = file_path.with_extension(format!("txt.claim.{}", std::process::id()));
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                did_work = true;
                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                let _ = fs::remove_file(&claim_path);
                claimed.push(ClaimedRequest { verb: verb.clone(), task, body });
            }
        }
    }

    let mut gm_requests: Vec<ClaimedRequest> = Vec::with_capacity(claimed.len());
    let mut bg_convert_requests: Vec<ClaimedRequest> = Vec::new();
    for req in claimed {
        if req.verb == "background-convert" {
            bg_convert_requests.push(req);
        } else {
            gm_requests.push(req);
        }
    }

    let answer_bg_converts = |reqs: Vec<ClaimedRequest>| {
        for req in reqs {
            let out_body = handle_background_convert(root, &req.body);
            let out_name = format!("{}-{}.json", req.verb, req.task);
            let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
            if fs::write(&tmp, &out_body).is_ok() {
                let _ = fs::rename(&tmp, out_dir.join(&out_name));
                let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
            }
        }
    };

    if gm_requests.is_empty() {
        answer_bg_converts(bg_convert_requests);
    } else {
        for plugin_name in &requested_plugins {
            if project.is_loaded(plugin_name) {
                continue;
            }
            let Some(module) = plugin_modules.modules.get(plugin_name) else {
                eprintln!("[agentplug daemon] plugin {plugin_name} not yet compiled for {}: dispatch this thread's own get_or_compile could not run against the shared PluginModules from a worker thread -- see plugin_modules.get_or_compile() call in run_daemon's pre-chunk warm pass", root.display());
                continue;
            };
            if let Err(e) = project.load_plugin(&plugin_modules.engine, plugin_name, module) {
                eprintln!("[agentplug daemon] failed to instantiate plugin {plugin_name} for {}: {e:#}", root.display());
            }
        }

        if !project.is_loaded("gm") {
            for req in &gm_requests {
                let out_name = format!("{}-{}.json", req.verb, req.task);
                let out_body = serde_json::json!({"ok": false, "error": "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)", "verb": req.verb}).to_string();
                let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
                if fs::write(&tmp, &out_body).is_ok() {
                    let _ = fs::rename(&tmp, out_dir.join(&out_name));
                    let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
                }
            }
            answer_bg_converts(bg_convert_requests);
        } else {
            struct Spawned {
                key: InFlightKey,
                join_handle: Option<std::thread::JoinHandle<()>>,
                detach_flag: Arc<std::sync::atomic::AtomicBool>,
            }
            let mut spawned: Vec<Spawned> = Vec::with_capacity(gm_requests.len());
            for req in gm_requests {
                let dispatch_handle = project.dispatch_handle();
                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
                in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                let thread_root = root.to_path_buf();
                let thread_verb = req.verb.clone();
                let thread_task = req.task.clone();
                let thread_body = req.body.clone();
                let thread_out_dir = out_dir.clone();
                let join_handle = std::thread::spawn(move || {
                    run_gm_dispatch_to_file(&thread_root, &dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir);
                });
                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag });
            }

            answer_bg_converts(bg_convert_requests);

            let bg_convert_dir = in_dir.join("background-convert");
            while spawned.iter().any(|s| s.join_handle.is_some()) {
                for s in spawned.iter_mut() {
                    let Some(jh) = s.join_handle.as_ref() else { continue };
                    if jh.is_finished() {
                        let jh = s.join_handle.take().unwrap();
                        let _ = jh.join();
                        in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&s.key);
                    } else if s.detach_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        s.join_handle = None;
                    }
                }
                if spawned.iter().any(|s| s.join_handle.is_some()) {
                    if let Ok(files) = fs::read_dir(&bg_convert_dir) {
                        for file_entry in files.flatten() {
                            let file_path = file_entry.path();
                            if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                                continue;
                            }
                            let claim_path = file_path.with_extension(format!("txt.claim.{}", std::process::id()));
                            if fs::rename(&file_path, &claim_path).is_err() {
                                continue;
                            }
                            let bc_task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                            let bc_body = fs::read_to_string(&claim_path).unwrap_or_default();
                            let _ = fs::remove_file(&claim_path);
                            let out_body = handle_background_convert(root, &bc_body);
                            let out_name = format!("background-convert-{bc_task}.json");
                            let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
                            if fs::write(&tmp, &out_body).is_ok() {
                                let _ = fs::rename(&tmp, out_dir.join(&out_name));
                                let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
                            }
                        }
                    }

                    if let Ok(verb_dirs) = fs::read_dir(&in_dir) {
                        for verb_entry in verb_dirs.flatten() {
                            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                continue;
                            }
                            let verb = verb_entry.file_name().to_string_lossy().into_owned();
                            if verb == "background-convert" {
                                continue;
                            }
                            let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
                            for file_entry in files.flatten() {
                                let file_path = file_entry.path();
                                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                                    continue;
                                }
                                let claim_path = file_path.with_extension(format!("txt.claim.{}", std::process::id()));
                                if fs::rename(&file_path, &claim_path).is_err() {
                                    continue;
                                }
                                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                                let _ = fs::remove_file(&claim_path);

                                let dispatch_handle = project.dispatch_handle();
                                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let key: InFlightKey = (root.to_path_buf(), verb.clone(), task.clone());
                                in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                                let thread_root = root.to_path_buf();
                                let thread_verb = verb.clone();
                                let thread_task = task.clone();
                                let thread_body = body;
                                let thread_out_dir = out_dir.clone();
                                let join_handle = std::thread::spawn(move || {
                                    run_gm_dispatch_to_file(&thread_root, &dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir);
                                });
                                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag });
                            }
                        }
                    }

                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    let pd_dir = root.join(".agentplug").join("plugin-dispatch");
    let pd_in = pd_dir.join("in");
    let pd_out = pd_dir.join("out");
    if fs::create_dir_all(&pd_in).is_err() || fs::create_dir_all(&pd_out).is_err() {
        return did_work;
    }
    let Ok(plugin_dirs) = fs::read_dir(&pd_in) else { return did_work };
    for plugin_entry in plugin_dirs.flatten() {
        if !plugin_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let plugin_name = plugin_entry.file_name().to_string_lossy().into_owned();
        let Ok(verb_dirs) = fs::read_dir(plugin_entry.path()) else { continue };
        for verb_entry in verb_dirs.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                    continue;
                }
                let claim_path = file_path.with_extension(format!("txt.claim.{}", std::process::id()));
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                did_work = true;
                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                let _ = fs::remove_file(&claim_path);

                let write_pd_out = |out_name: &str, out_body: &str| {
                    let tmp = pd_out.join(format!("{out_name}.tmp.{}", std::process::id()));
                    if fs::write(&tmp, out_body).is_ok() {
                        let _ = fs::rename(&tmp, pd_out.join(out_name));
                        let _ = fs::write(pd_out.join(format!("{out_name}.ready")), b"");
                    }
                };

                if !project.is_loaded(&plugin_name) {
                    let Some(module) = plugin_modules.modules.get(&plugin_name) else {
                        let out_name = format!("{plugin_name}-{verb}-{task}.json");
                        let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                        write_pd_out(&out_name, &out_body);
                        continue;
                    };
                    if let Err(e) = project.load_plugin(&plugin_modules.engine, &plugin_name, module) {
                        let out_name = format!("{plugin_name}-{verb}-{task}.json");
                        let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                        write_pd_out(&out_name, &out_body);
                        continue;
                    }
                }

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| project.dispatch(&plugin_name, &verb, &body)));
                let out_name = format!("{plugin_name}-{verb}-{task}.json");
                let out_body = match result {
                    Ok(Ok(s)) if !s.is_empty() => s,
                    Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result"}).to_string(),
                    Ok(Err(e)) => serde_json::json!({"ok": false, "error": format!("{e:#}")}).to_string(),
                    Err(panic_payload) => {
                        let msg = panic_payload
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic with non-string payload".to_string());
                        eprintln!("[agentplug daemon] plugin {plugin_name} verb {verb} PANICKED for {}: {msg}", root.display());
                        serde_json::json!({"ok": false, "error": format!("dispatch panicked: {msg}"), "verb": verb}).to_string()
                    }
                };
                write_pd_out(&out_name, &out_body);
            }
        }
    }

    did_work
}

pub fn try_dispatch_via_daemon(cwd: &Path, plugin: &str, verb: &str, body: &str) -> Option<String> {
    if std::env::var("AGENTPLUG_NO_DAEMON").is_ok() {
        return None;
    }
    if register_project(cwd).is_err() {
        return None;
    }
    if !ensure_daemon_running().unwrap_or(false) {
        return None;
    }

    let pd_dir = cwd.join(".agentplug").join("plugin-dispatch");
    let in_dir = pd_dir.join("in").join(plugin).join(verb);
    let out_dir = pd_dir.join("out");
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return None;
    }

    let task = format!("{}{}", std::process::id(), now_ms());
    let req_path = in_dir.join(format!("{task}.txt"));
    if fs::write(&req_path, body).is_err() {
        return None;
    }
    let out_path = out_dir.join(format!("{plugin}-{verb}-{task}.json"));

    const POLL_INTERVAL_MS: u64 = 100;
    const MAX_WAIT_MS: u64 = 30_000;
    let mut waited = 0u64;
    while waited < MAX_WAIT_MS {
        if let Ok(content) = fs::read_to_string(&out_path) {
            let _ = fs::remove_file(&out_path);
            return Some(content);
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        waited += POLL_INTERVAL_MS;
    }
    let _ = fs::remove_file(&req_path);
    None
}

pub fn run_daemon() -> anyhow::Result<()> {
    eprintln!("[agentplug daemon] starting, registry {}", registry_path().display());

    if !claim_ownership() {
        let existing_pid = read_owner_pid();
        eprintln!(
            "[agentplug daemon] lost the atomic ownership claim -- pid {:?} already owns the shared daemon, exiting before touching any shared plugin state",
            existing_pid
        );
        return Ok(());
    }

    let plugin_modules = PluginModules::new()?;
    if installed_runner_version().is_none() {
        let _ = record_runner_version(env!("CARGO_PKG_VERSION"));
    }
    run_daemon_body(plugin_modules)
}

fn run_daemon_body(mut plugin_modules: PluginModules) -> anyhow::Result<()> {
    write_daemon_heartbeat(0, 0);

    let daemon_cfg = DaemonConfig::load();
    let registry_poll_interval = daemon_cfg.registry_poll_interval();
    let heartbeat_interval = daemon_cfg.heartbeat_interval();
    agentplug_host::set_gm_pool_size(daemon_cfg.gm_concurrency());
    agentplug_host::set_side_plugin_pool_size(daemon_cfg.side_plugin_concurrency());

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now();
    let mut first_registry_poll_pending = true;
    let mut known_roots: Vec<PathBuf> = Vec::new();

    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    const SHARED_PLUGIN_RELEASE_IDLE_MS: u64 = 2 * 60 * 1000;
    let mut last_shared_release = Instant::now();

    let plugin_update_poll_interval = daemon_cfg.plugin_update_poll_interval();
    let mut last_plugin_update_poll = Instant::now();

    let runner_update_poll_interval = daemon_cfg.runner_update_poll_interval();
    let mut last_runner_update_poll = Instant::now();
    let mut pending_self_update: Option<(PathBuf, String)> = None;
    let mut pending_plugin_swaps: Vec<(String, String)> = Vec::new();

    let mut last_instruction_source_sync: HashMap<PathBuf, Instant> = HashMap::new();

    let _heartbeat_ticker = spawn_heartbeat_ticker(heartbeat_interval);
    write_daemon_heartbeat(0, 0);

    loop {
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting before serving further work");
            return Ok(());
        }

        if first_registry_poll_pending || last_registry_poll.elapsed() >= registry_poll_interval {
            first_registry_poll_pending = false;
            last_registry_poll = Instant::now();
            known_roots = read_registry();
        }

        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

        for root in &known_roots {
            for plugin_name in read_project_plugin_list(root) {
                if let Err(e) = plugin_modules.get_or_compile(&plugin_name) {
                    eprintln!("[agentplug daemon] failed to compile/install plugin {plugin_name} for {}: {e:#}", root.display());
                }
            }
            let due = last_instruction_source_sync
                .get(root)
                .map(|t| t.elapsed() >= plugin_update_poll_interval)
                .unwrap_or(true);
            if due {
                last_instruction_source_sync.insert(root.clone(), Instant::now());
                if let Err(e) = sync_instruction_source_if_configured(root) {
                    eprintln!("[agentplug daemon] instruction source-repo sync failed for {}: {e:#}", root.display());
                }
            }
        }
        for plugin_name in ["gm", "libsql", "bert", "treesitter"] {
            if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
                eprintln!("[agentplug daemon] failed to compile/install default plugin {plugin_name}: {e:#}");
            }
        }

        let all_projects: Vec<(PathBuf, ProjectPlugins)> = known_roots
            .iter()
            .map(|root| {
                let p = projects.remove(root).unwrap_or_else(|| ProjectPlugins::new(root.clone()));
                (root.clone(), p)
            })
            .collect();
        let worker_count = max_concurrent_projects.min(all_projects.len().max(1));
        let queue = std::sync::Mutex::new(all_projects);
        let done = std::sync::Mutex::new(Vec::<(PathBuf, ProjectPlugins, bool)>::new());
        {
            let plugin_modules_ref: &PluginModules = &plugin_modules;
            let queue_ref = &queue;
            let done_ref = &done;
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(worker_count);
                for _ in 0..worker_count {
                    handles.push(scope.spawn(move || loop {
                        let next = { queue_ref.lock().unwrap_or_else(|e| e.into_inner()).pop() };
                        let Some((root, mut project)) = next else { break };
                        let did_work = dispatch_project(root.as_path(), &mut project, plugin_modules_ref);
                        done_ref.lock().unwrap_or_else(|e| e.into_inner()).push((root, project, did_work));
                    }));
                }
                for h in handles { let _ = h.join(); }
            });
        }
        let mut any_work = false;
        for (root, project, did_work) in done.into_inner().unwrap_or_else(|e| e.into_inner()) {
            any_work = any_work || did_work;
            projects.insert(root, project);
        }
        HEARTBEAT_PROJECT_COUNT.store(projects.len(), std::sync::atomic::Ordering::Relaxed);
        HEARTBEAT_PLUGIN_MODULE_COUNT.store(plugin_modules.modules.len(), std::sync::atomic::Ordering::Relaxed);
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            return Ok(());
        }
        let evict_before = Instant::now().checked_sub(Duration::from_millis(PLUGIN_IDLE_EVICT_MS)).unwrap_or_else(Instant::now);
        let to_evict: Vec<PathBuf> = projects.iter().filter(|(_, p)| p.last_active < evict_before).map(|(root, _)| root.clone()).collect();
        for root in to_evict {
            eprintln!("[agentplug daemon] evicting idle project {}", root.display());
            projects.remove(&root);
        }

        if last_plugin_update_poll.elapsed() >= plugin_update_poll_interval {
            last_plugin_update_poll = Instant::now();
            for plugin_name in plugin_modules.modules.keys().cloned().collect::<Vec<_>>() {
                match crate::download::refresh_plugin_if_stale(&plugin_name) {
                    Ok(Some(new_version)) => {
                        eprintln!(
                            "[agentplug daemon] downloaded+verified plugin {plugin_name} update to {new_version} -- queued for live-swap on next idle tick"
                        );
                        pending_plugin_swaps.push((plugin_name, new_version));
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[agentplug daemon] plugin update check for {plugin_name} failed: {e}"),
                }
            }
        }

        if !any_work && !pending_plugin_swaps.is_empty() {
            for (plugin_name, new_version) in pending_plugin_swaps.drain(..) {
                plugin_modules.modules.remove(&plugin_name);
                agentplug_host::release_shared_plugin(&plugin_name);
                eprintln!(
                    "[agentplug daemon] refreshed plugin {plugin_name} to {new_version} -- released its Store; next call re-instantiates from the new wasm"
                );
            }
        }

        if last_runner_update_poll.elapsed() >= runner_update_poll_interval {
            last_runner_update_poll = Instant::now();
            match crate::download::stage_runner_self_update() {
                Ok(Some((staged, version))) => {
                    eprintln!("[agentplug daemon] staged self-update to {version} at {}", staged.display());
                    pending_self_update = Some((staged, version));
                }
                Ok(None) => {}
                Err(e) => eprintln!("[agentplug daemon] runner self-update check failed: {e}"),
            }
        }

        if !any_work {
            if let Some((staged, version)) = pending_self_update.take() {
                if attempt_self_update_handoff(&staged, &version) {
                    agentplug_host::close_all_sessions();
                    eprintln!("[agentplug daemon] handed off to version {version} -- exiting");
                    return Ok(());
                }
            }
        }

        if any_work {
            last_shared_release = Instant::now();
        } else if last_shared_release.elapsed() >= Duration::from_millis(SHARED_PLUGIN_RELEASE_IDLE_MS) {
            let mut released: Vec<&str> = Vec::new();
            for plugin_name in ["bert", "treesitter", "libsql"] {
                if agentplug_host::release_shared_plugin(plugin_name) {
                    released.push(plugin_name);
                }
            }
            if !released.is_empty() {
                eprintln!(
                    "[agentplug daemon] released idle shared Stores [{}] after {}ms quiet -- returns their grown wasm linear memory; next call re-instantiates",
                    released.join(", "),
                    SHARED_PLUGIN_RELEASE_IDLE_MS
                );
            }
            last_shared_release = Instant::now();
        }

        if any_work {
            last_any_dispatch = Instant::now();
        } else if last_any_dispatch.elapsed() >= Duration::from_millis(SELF_RECYCLE_IDLE_MS) {
            eprintln!(
                "[agentplug daemon] self-recycling after {}ms fully idle -- reclaims shared-plugin peak wasm memory (monotonic linear memory, no in-place shrink); next real dispatch spawns a fresh process",
                SELF_RECYCLE_IDLE_MS
            );
            return Ok(());
        }

        if !any_work {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
