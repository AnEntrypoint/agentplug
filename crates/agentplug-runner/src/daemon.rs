use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use wasmtime::{Engine, Module, Trap};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, DispatchHandle, GmFairnessGuard, ProjectPlugins, PLUGIN_IDLE_EVICT_MS};

use crate::download::{ensure_plugin_installed, installed_plugin_version, installed_runner_version, is_recognized_release_semver, record_runner_version};

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

    let mut live: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for line in existing.lines() {
        let entry = line.trim();
        if entry.is_empty() || live.iter().any(|e| e == entry) {
            continue;
        }
        if entry == cwd_str || Path::new(entry).exists() {
            live.push(entry.to_string());
        } else {
            dropped += 1;
        }
    }

    let already_present = live.iter().any(|e| e == &cwd_str);
    if already_present && dropped == 0 {
        return Ok(());
    }
    if !already_present {
        live.push(cwd_str);
    }

    let mut body = live.join("\n");
    body.push('\n');
    let tmp = path.with_extension("txt.tmp");
    fs::write(&tmp, &body)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(e: &anyhow::Error) -> String {
    match e.downcast_ref::<Trap>() {
        Some(trap) => format!("[wasm trap: {trap}] {e:#}"),
        None => format!("{e:#}"),
    }
}

pub(crate) fn read_registry() -> Vec<PathBuf> {
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
    #[serde(default)]
    shared_store_recycle_private_mb: Option<u64>,
    #[serde(default)]
    shared_store_recycle_dispatches: Option<u64>,
}

const DAEMON_CONFIG_EXAMPLE: &str = r#"{
  "registry_poll_interval_secs": 5,
  "heartbeat_interval_secs": 10,
  "plugin_update_poll_interval_secs": 600,
  "runner_update_poll_interval_secs": 600,
  "max_concurrent_projects": 4,
  "gm_concurrency": 4,
  "side_plugin_concurrency": 1,
  "shared_store_recycle_private_mb": 1600,
  "shared_store_recycle_dispatches": 2000
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
        let raw = fs::read_to_string(&path).ok();
        if let Some(text) = raw.as_deref() {
            let cleaned = text.trim_start_matches('\u{feff}');
            match serde_json::from_str::<DaemonConfig>(cleaned) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    eprintln!(
                        "[agentplug daemon] {} exists but failed to parse ({e}); EVERY setting in it is being ignored and compiled defaults are in force",
                        path.display()
                    );
                }
            }
        }
        DaemonConfig {
                registry_poll_interval_secs: None,
                heartbeat_interval_secs: None,
                plugin_update_poll_interval_secs: None,
                runner_update_poll_interval_secs: None,
                max_concurrent_projects: None,
                gm_concurrency: None,
                side_plugin_concurrency: None,
                shared_store_recycle_private_mb: None,
                shared_store_recycle_dispatches: None,
            }
    }
    fn registry_poll_interval(&self) -> Duration { Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5)) }
    fn heartbeat_interval(&self) -> Duration { Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10)) }
    fn plugin_update_poll_interval(&self) -> Duration { Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600)) }
    fn runner_update_poll_interval(&self) -> Duration { Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(600)) }
    fn max_concurrent_projects(&self) -> usize { self.max_concurrent_projects.unwrap_or(4).max(1) }
    fn gm_concurrency(&self) -> usize { self.gm_concurrency.unwrap_or_else(|| self.max_concurrent_projects()).max(1) }
    fn side_plugin_concurrency(&self) -> usize { self.side_plugin_concurrency.unwrap_or(1).max(1) }
    fn shared_store_recycle_private_bytes(&self) -> u64 { self.shared_store_recycle_private_mb.unwrap_or(1600).max(256) * 1024 * 1024 }
    fn shared_store_recycle_dispatches(&self) -> u64 { self.shared_store_recycle_dispatches.unwrap_or(2000).max(1) }
}

fn shared_store_recycle_reason_independent_of_daemon_idle_state(cfg: &DaemonConfig) -> Option<String> {
    let dispatches = agentplug_host::shared_dispatches_since_release();
    if let Some(private_bytes) = agentplug_host::process_private_bytes_tracking_retained_wasm_peak_unlike_working_set() {
        let limit = cfg.shared_store_recycle_private_bytes();
        if private_bytes >= limit {
            return Some(format!(
                "memory pressure: {}MB private commit >= {}MB limit (after {dispatches} shared dispatches)",
                private_bytes / (1024 * 1024),
                limit / (1024 * 1024)
            ));
        }
    }
    let dispatch_limit = cfg.shared_store_recycle_dispatches();
    if dispatches >= dispatch_limit {
        return Some(format!("dispatch budget: {dispatches} shared dispatches >= {dispatch_limit} limit"));
    }
    None
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
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
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

fn run_git_bounded(args: &[&str]) -> anyhow::Result<std::process::Output> {
    use wait_timeout::ChildExt;
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let timeout_ms = agentplug_host::git_subprocess_timeout_ms();
    match child.wait_timeout(Duration::from_millis(timeout_ms))? {
        Some(status) => {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut o) = child.stdout.take() { let _ = std::io::Read::read_to_end(&mut o, &mut stdout); }
            if let Some(mut e) = child.stderr.take() { let _ = std::io::Read::read_to_end(&mut e, &mut stderr); }
            Ok(std::process::Output { status, stdout, stderr })
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("git {args:?} exceeded {timeout_ms}ms with no completion -- killed to avoid wedging the daemon's own main loop, which runs this call sequentially ahead of every project's dispatch");
        }
    }
}

fn sync_instruction_source_if_configured(root: &Path) -> anyhow::Result<()> {
    let config_path = instruction_source_config_path(root);
    let Ok(raw) = fs::read_to_string(&config_path) else { return Ok(()) };
    let Ok(cfg) = serde_json::from_str::<InstructionSourceConfig>(&raw) else {
        eprintln!("[agentplug daemon] {} exists but does not parse as {{repo, branch?, path?}} -- ignoring", config_path.display());
        return Ok(());
    };
    let cache_dir = instruction_source_cache_dir(root);
    let cache_dir_str = cache_dir.to_string_lossy().into_owned();
    let git_dir_marker = cache_dir.join(".git");
    if !git_dir_marker.exists() {
        fs::create_dir_all(root.join(".gm"))?;
        let output = run_git_bounded(&["clone", "--depth", "1", "--branch", &cfg.branch, &cfg.repo, &cache_dir_str])?;
        if !output.status.success() {
            anyhow::bail!("git clone of {} (branch {}) failed", cfg.repo, cfg.branch);
        }
        eprintln!("[agentplug daemon] cloned instruction source {} (branch {}) for {}", cfg.repo, cfg.branch, root.display());
        return Ok(());
    }
    let fetch = run_git_bounded(&["-C", &cache_dir_str, "fetch", "--depth", "1", "origin", &cfg.branch])?;
    if !fetch.status.success() {
        anyhow::bail!("git fetch of {} (branch {}) failed", cfg.repo, cfg.branch);
    }
    let reset_target = format!("origin/{}", cfg.branch);
    let reset = run_git_bounded(&["-C", &cache_dir_str, "reset", "--hard", &reset_target])?;
    if !reset.status.success() {
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
    let last_plugin_poll_ts = HEARTBEAT_LAST_PLUGIN_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let last_runner_poll_ts = HEARTBEAT_LAST_RUNNER_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let loaded_content_hashes: HashMap<String, String> =
        loaded_plugin_content_hashes().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let shared_pool_slot_hashes: HashMap<String, Vec<Option<String>>> = ["gm", "bert", "libsql", "treesitter"]
        .iter()
        .map(|name| (name.to_string(), agentplug_host::shared_plugin_slot_content_hashes(name)))
        .collect();
    let mixed_version_pools: Vec<String> = shared_pool_slot_hashes
        .iter()
        .filter(|(_, hashes)| hashes.iter().flatten().collect::<std::collections::HashSet<_>>().len() > 1)
        .map(|(name, _)| name.clone())
        .collect();
    let boot_ts = HEARTBEAT_DAEMON_BOOT_TS.load(std::sync::atomic::Ordering::Relaxed);
    let plugin_poll_error = last_plugin_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let runner_poll_error = last_runner_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let staged_runner = staged_runner_awaiting_handoff();
    let _ = fs::write(
        daemon_status_path(),
        serde_json::json!({
            "pid": std::process::id(),
            "ts": now_ms(),
            "daemon_boot_ts": if boot_ts == 0 { serde_json::Value::Null } else { serde_json::json!(boot_ts) },
            "active_projects": project_count,
            "compiled_plugin_modules": plugin_module_count,
            "last_plugin_update_poll_ts": if last_plugin_poll_ts == 0 { serde_json::Value::Null } else { serde_json::json!(last_plugin_poll_ts) },
            "last_runner_update_poll_ts": if last_runner_poll_ts == 0 { serde_json::Value::Null } else { serde_json::json!(last_runner_poll_ts) },
            "last_plugin_update_poll_error": plugin_poll_error,
            "last_runner_update_poll_error": runner_poll_error,
            "loaded_plugin_content_sha256": loaded_content_hashes,
            "shared_pool_slot_content_sha256": shared_pool_slot_hashes,
            "mixed_version_pools": mixed_version_pools,
            "staged_runner_awaiting_handoff": staged_runner.is_some(),
            "staged_runner_since_ts": staged_runner.map(|(since_ts, _)| serde_json::json!(since_ts)).unwrap_or(serde_json::Value::Null),
            "staged_runner_waiting_ms": staged_runner.map(|(since_ts, _)| serde_json::json!(now_ms().saturating_sub(since_ts))).unwrap_or(serde_json::Value::Null),
        })
        .to_string(),
    );
}

fn canonical_runner_exe_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    while path.extension().map(|e| e.eq_ignore_ascii_case("new")).unwrap_or(false) {
        path = path.with_extension("");
    }
    Some(path)
}

fn staged_runner_awaiting_handoff() -> Option<(u64, u64)> {
    let canonical = canonical_runner_exe_path()?;
    let staged = canonical.with_extension(
        canonical.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string()),
    );
    let meta = fs::metadata(&staged).ok()?;
    let staged_at_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((staged_at_ms, meta.len()))
}

fn write_project_heartbeat(spool_dir: &Path, busy_until: Option<u64>) {
    let status_path = spool_dir.join(".status.json");
    let mut payload = serde_json::json!({
        "pid": std::process::id(),
        "ts": now_ms(),
        "daemon": true,
        "shared_process": true,
        "runtime": "agentplug",
    });
    if let Some(busy_until) = busy_until {
        payload["busy_until"] = serde_json::json!(busy_until);
    }
    let _ = fs::write(&status_path, payload.to_string());
}

fn known_project_roots() -> &'static Mutex<Vec<PathBuf>> {
    static SLOT: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_known_project_roots(roots: &[PathBuf]) {
    *known_project_roots().lock().unwrap_or_else(|e| e.into_inner()) = roots.to_vec();
}

fn spawn_project_heartbeat_ticker(interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        if heartbeat_authority_lost() {
            return;
        }
        let roots = known_project_roots().lock().unwrap_or_else(|e| e.into_inner()).clone();
        for root in roots {
            let spool_dir = root.join(".gm").join("exec-spool");
            if !spool_dir.exists() {
                continue;
            }
            write_project_heartbeat(&spool_dir, None);
        }
    })
}

static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_LAST_PLUGIN_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_LAST_RUNNER_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_DAEMON_BOOT_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn last_plugin_poll_error() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn last_runner_poll_error() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_plugin_poll_error(err: Option<String>) {
    *last_plugin_poll_error().lock().unwrap_or_else(|e| e.into_inner()) = err;
}

fn record_runner_poll_error(err: Option<String>) {
    *last_runner_poll_error().lock().unwrap_or_else(|e| e.into_inner()) = err;
}

fn persisted_plugin_poll_ts_path() -> PathBuf {
    install_dir().join("last-plugin-update-poll-ts")
}

fn persisted_runner_poll_ts_path() -> PathBuf {
    install_dir().join("last-runner-update-poll-ts")
}

fn read_persisted_poll_ts(path: &Path) -> u64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)
}

fn write_persisted_poll_ts(path: &Path, ts: u64) {
    let _ = fs::create_dir_all(install_dir());
    let _ = fs::write(path, ts.to_string());
}

fn instant_backdated_by_ms_capped_to_process_epoch(ms_ago: u64) -> Instant {
    let now = Instant::now();
    let mut probe = ms_ago;
    while probe > 0 {
        if let Some(candidate) = now.checked_sub(Duration::from_millis(probe)) {
            return candidate;
        }
        probe /= 2;
    }
    now
}

fn seed_poll_timer_from_persisted_ts(path: &Path) -> Instant {
    const NEVER_POLLED_BACKDATE_MS: u64 = 365 * 24 * 60 * 60 * 1000;
    let persisted_ts = read_persisted_poll_ts(path);
    let elapsed_ms = if persisted_ts == 0 { NEVER_POLLED_BACKDATE_MS } else { now_ms().saturating_sub(persisted_ts) };
    instant_backdated_by_ms_capped_to_process_epoch(elapsed_ms)
}
static LOADED_PLUGIN_CONTENT_HASHES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn loaded_plugin_content_hashes() -> &'static Mutex<HashMap<String, String>> {
    LOADED_PLUGIN_CONTENT_HASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

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
    loaded_content_hash: HashMap<String, String>,
}

fn wasm_file_content_hash(wasm_path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(wasm_path)?;
    Ok(crate::download::sha256_hex(&bytes))
}

impl PluginModules {
    fn new() -> anyhow::Result<Self> {
        Ok(Self { engine: build_engine()?, modules: HashMap::new(), loaded_content_hash: HashMap::new() })
    }

    fn get_or_compile(&mut self, plugin_name: &str) -> anyhow::Result<()> {
        let wasm_path = ensure_plugin_installed(plugin_name, None)?;
        let on_disk_hash = wasm_file_content_hash(&wasm_path)?;
        let stale = self
            .loaded_content_hash
            .get(plugin_name)
            .is_some_and(|loaded_hash| loaded_hash != &on_disk_hash);
        if stale {
            eprintln!(
                "[agentplug daemon] {plugin_name}.wasm content hash changed on disk since it was last compiled -- evicting the stale in-process module and the shared Store using it, forcing a recompile from the current bytes"
            );
            self.modules.remove(plugin_name);
            agentplug_host::release_shared_plugin(plugin_name);
        }
        if !self.modules.contains_key(plugin_name) {
            if let Some(installed) = installed_plugin_version(plugin_name) {
                if !is_recognized_release_semver(&installed) {
                    eprintln!(
                        "[agentplug daemon] BOOT WARNING: {plugin_name}.wasm at {} is served from a NON-RELEASE version marker ({installed:?}) -- this is a local-dev sideload, not a released build, and the auto-updater will never overwrite it. If this was not intentional, replace the sideload with a real release-tagged {plugin_name}.wasm.",
                        wasm_path.display()
                    );
                }
            }
            eprintln!("[agentplug daemon] compiling {plugin_name}.wasm (shared across every project that uses it)...");
            let module = Module::from_file(&self.engine, &wasm_path)?;
            self.modules.insert(plugin_name.to_string(), module);
            self.loaded_content_hash.insert(plugin_name.to_string(), on_disk_hash.clone());
            loaded_plugin_content_hashes()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(plugin_name.to_string(), on_disk_hash);
        }
        Ok(())
    }

    fn module_with_hash(&self, plugin_name: &str) -> Option<(&Module, &str)> {
        let module = self.modules.get(plugin_name)?;
        let hash = self.loaded_content_hash.get(plugin_name)?;
        Some((module, hash.as_str()))
    }

    fn modules_with_hashes(&self) -> HashMap<String, (Module, String)> {
        self.modules
            .iter()
            .filter_map(|(name, module)| {
                let hash = self.loaded_content_hash.get(name)?;
                Some((name.clone(), (module.clone(), hash.clone())))
            })
            .collect()
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

fn handle_plugin_refresh_request(root: &Path, body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let requested_plugin = parsed.as_ref().and_then(|v| v.get("plugin").and_then(|p| p.as_str()).map(str::to_string));
    let also_runner = parsed.as_ref().and_then(|v| v.get("runner").and_then(|r| r.as_bool())).unwrap_or(false);

    let marker = force_plugin_refresh_marker_path();
    let contents = requested_plugin.as_deref().unwrap_or("").to_string();
    let _ = fs::write(&marker, contents);

    if also_runner {
        let _ = fs::write(force_runner_refresh_marker_path(), b"");
    }

    let local_dev_sideload = requested_plugin.as_deref().and_then(crate::download::read_local_dev_sideload_marker);

    serde_json::json!({
        "ok": true,
        "queued": true,
        "plugin": requested_plugin,
        "runner_queued": also_runner,
        "local_dev_sideload": local_dev_sideload,
        "note": "the running daemon's plugin-update (and, if runner:true was passed, runner-binary-update) poll will fire on its next loop tick instead of waiting for the normal interval; re-dispatch health shortly after to observe the new version. local_dev_sideload is non-null only when the queried plugin's installed .version marker is not recognized release semver -- that plugin will never be auto-updated until the marker or the wasm is replaced",
        "root": root.display().to_string(),
    }).to_string()
}

fn force_plugin_refresh_marker_path() -> PathBuf {
    install_dir().join("force-plugin-refresh.request")
}

fn take_forced_plugin_refresh_request() -> Option<Option<String>> {
    let marker = force_plugin_refresh_marker_path();
    let contents = fs::read_to_string(&marker).ok()?;
    let _ = fs::remove_file(&marker);
    Some(if contents.trim().is_empty() { None } else { Some(contents.trim().to_string()) })
}

fn force_runner_refresh_marker_path() -> PathBuf {
    install_dir().join("force-runner-refresh.request")
}

fn take_forced_runner_refresh_request() -> bool {
    let marker = force_runner_refresh_marker_path();
    if marker.exists() {
        let _ = fs::remove_file(&marker);
        true
    } else {
        false
    }
}

fn write_spool_out(out_dir: &Path, out_name: &str, out_body: &str) {
    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
    if fs::write(&tmp, out_body).is_ok() {
        let _ = fs::rename(&tmp, out_dir.join(out_name));
        let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
    }
}

const ORPHAN_CLAIM_EXT: &str = "inflight";

fn inflight_claim_path(in_dir: &Path, verb: &str, task: &str) -> PathBuf {
    in_dir.join(verb).join(format!("{task}.txt.{ORPHAN_CLAIM_EXT}"))
}

fn sweep_orphaned_claims(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else { return };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(ORPHAN_CLAIM_EXT) {
                continue;
            }
            let task = Path::new(path.file_stem().unwrap_or_default())
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if task.is_empty() {
                let _ = fs::remove_file(&path);
                continue;
            }
            let out_name = format!("{verb}-{task}.json");
            if !out_dir.join(&out_name).exists() {
                let out_body = serde_json::json!({
                    "ok": false,
                    "error_code": "dispatch_orphaned",
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that died before answering -- most likely a wasm trap, an out-of-memory abort, or a shared-Store recycle during the call. The request was NOT completed and no partial work should be assumed. Re-dispatch it."),
                    "verb": verb,
                    "task": task,
                }).to_string();
                write_spool_out(&out_dir, &out_name, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
            }
            let _ = fs::remove_file(&path);
        }
    }
}

fn run_gm_dispatch_to_file(root: &Path, handle: &DispatchHandle, verb: &str, task: &str, body: &str, out_dir: &Path) {
    let _fairness_guard = GmFairnessGuard::acquire(root);
    let dispatch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch("gm", verb, body)));
    let out_body = match dispatch_result {
        Ok(Ok(s)) if !s.is_empty() => s,
        Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
        Ok(Err(e)) => serde_json::json!({"ok": false, "error": describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(&e), "verb": verb}).to_string(),
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
    write_spool_out(out_dir, &out_name, &out_body);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
}

fn dispatch_project(root: &Path, project: &mut ProjectPlugins, plugin_modules: &PluginModules) -> bool {
    let mut did_work = false;

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }

    write_project_heartbeat(&spool_dir, None);

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
                let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                did_work = true;
                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                claimed.push(ClaimedRequest { verb: verb.clone(), task, body });
            }
        }
    }

    let mut gm_requests: Vec<ClaimedRequest> = Vec::with_capacity(claimed.len());
    let mut bg_convert_requests: Vec<ClaimedRequest> = Vec::new();
    let mut plugin_refresh_requests: Vec<ClaimedRequest> = Vec::new();
    for req in claimed {
        if req.verb == "background-convert" {
            bg_convert_requests.push(req);
        } else if req.verb == "plugin-refresh" {
            plugin_refresh_requests.push(req);
        } else {
            gm_requests.push(req);
        }
    }

    let answer_bg_converts = |reqs: Vec<ClaimedRequest>| {
        for req in reqs {
            let out_body = handle_background_convert(root, &req.body);
            let out_name = format!("{}-{}.json", req.verb, req.task);
            write_spool_out(&out_dir, &out_name, &out_body);
            let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
        }
    };
    for req in plugin_refresh_requests {
        let out_body = handle_plugin_refresh_request(root, &req.body);
        let out_name = format!("{}-{}.json", req.verb, req.task);
        write_spool_out(&out_dir, &out_name, &out_body);
        let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
        did_work = true;
    }

    if gm_requests.is_empty() {
        answer_bg_converts(bg_convert_requests);
    } else {
        for plugin_name in &requested_plugins {
            if project.is_loaded(plugin_name) {
                continue;
            }
            let Some((module, content_hash)) = plugin_modules.module_with_hash(plugin_name) else {
                eprintln!("[agentplug daemon] plugin {plugin_name} not yet compiled for {}: dispatch this thread's own get_or_compile could not run against the shared PluginModules from a worker thread -- see plugin_modules.get_or_compile() call in run_daemon's pre-chunk warm pass", root.display());
                continue;
            };
            if let Err(e) = project.load_plugin(&plugin_modules.engine, plugin_name, module, content_hash) {
                eprintln!("[agentplug daemon] failed to instantiate plugin {plugin_name} for {}: {e:#}", root.display());
            }
        }

        if !project.is_loaded("gm") {
            for req in &gm_requests {
                let out_name = format!("{}-{}.json", req.verb, req.task);
                let out_body = serde_json::json!({"ok": false, "error": "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)", "verb": req.verb}).to_string();
                write_spool_out(&out_dir, &out_name, &out_body);
                let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
            }
            answer_bg_converts(bg_convert_requests);
        } else {
            struct Spawned {
                key: InFlightKey,
                join_handle: Option<std::thread::JoinHandle<()>>,
                detach_flag: Arc<std::sync::atomic::AtomicBool>,
                spawned_at: Instant,
            }
            let mut spawned: Vec<Spawned> = Vec::with_capacity(gm_requests.len());
            for req in gm_requests {
                let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((plugin_modules.engine.clone(), plugin_modules.modules_with_hashes())));
                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
                in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                let thread_root = root.to_path_buf();
                let thread_verb = req.verb.clone();
                let thread_task = req.task.clone();
                let thread_body = req.body.clone();
                let thread_out_dir = out_dir.clone();
                let join_handle = std::thread::spawn(move || {
                    run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir);
                });
                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
            }

            answer_bg_converts(bg_convert_requests);

            const WORKER_AUTO_DETACH_AFTER_MS: u64 = 45_000;
            const STATUS_REFRESH_INTERVAL_MS: u64 = 5_000;
            let mut last_status_refresh = Instant::now();
            let bg_convert_dir = in_dir.join("background-convert");
            while spawned.iter().any(|s| s.join_handle.is_some()) {
                if last_status_refresh.elapsed() >= Duration::from_millis(STATUS_REFRESH_INTERVAL_MS) {
                    last_status_refresh = Instant::now();
                    write_project_heartbeat(&spool_dir, Some(now_ms() + STATUS_REFRESH_INTERVAL_MS));
                }
                for s in spawned.iter_mut() {
                    if s.join_handle.is_some()
                        && !s.detach_flag.load(std::sync::atomic::Ordering::SeqCst)
                        && s.spawned_at.elapsed() >= Duration::from_millis(WORKER_AUTO_DETACH_AFTER_MS)
                    {
                        eprintln!(
                            "[agentplug daemon] gm dispatch for {} exceeded {WORKER_AUTO_DETACH_AFTER_MS}ms with no completion -- auto-detaching so this worker and the daemon's other projects are not blocked; it keeps running and will write its out/ file whenever it finishes",
                            root.display()
                        );
                        s.detach_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        s.join_handle = None;
                    }
                }
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
                            let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                            if fs::rename(&file_path, &claim_path).is_err() {
                                continue;
                            }
                            let bc_task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                            let bc_body = fs::read_to_string(&claim_path).unwrap_or_default();
                            let out_body = handle_background_convert(root, &bc_body);
                            let out_name = format!("background-convert-{bc_task}.json");
                            write_spool_out(&out_dir, &out_name, &out_body);
                            let _ = fs::remove_file(&claim_path);
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
                                let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                                if fs::rename(&file_path, &claim_path).is_err() {
                                    continue;
                                }
                                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                let body = fs::read_to_string(&claim_path).unwrap_or_default();

                                let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((plugin_modules.engine.clone(), plugin_modules.modules_with_hashes())));
                                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let key: InFlightKey = (root.to_path_buf(), verb.clone(), task.clone());
                                in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                                let thread_root = root.to_path_buf();
                                let thread_verb = verb.clone();
                                let thread_task = task.clone();
                                let thread_body = body;
                                let thread_out_dir = out_dir.clone();
                                let join_handle = std::thread::spawn(move || {
                                    run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir);
                                });
                                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
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
                    let Some((module, content_hash)) = plugin_modules.module_with_hash(&plugin_name) else {
                        let out_name = format!("{plugin_name}-{verb}-{task}.json");
                        let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                        write_pd_out(&out_name, &out_body);
                        continue;
                    };
                    if let Err(e) = project.load_plugin(&plugin_modules.engine, &plugin_name, module, content_hash) {
                        let out_name = format!("{plugin_name}-{verb}-{task}.json");
                        let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                        write_pd_out(&out_name, &out_body);
                        continue;
                    }
                }

                if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&DaemonConfig::load()) {
                    let mut released: Vec<&str> = Vec::new();
                    for shared_name in ["bert", "treesitter", "libsql"] {
                        if shared_name != plugin_name && agentplug_host::release_shared_plugin(shared_name) {
                            released.push(shared_name);
                        }
                    }
                    agentplug_host::reset_shared_dispatch_count();
                    if !released.is_empty() {
                        eprintln!(
                            "[agentplug daemon] pre-dispatch release of shared Stores {released:?} before {plugin_name}/{verb} -- {reason}"
                        );
                    }
                }

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| project.dispatch(&plugin_name, &verb, &body)));
                let out_name = format!("{plugin_name}-{verb}-{task}.json");
                let out_body = match result {
                    Ok(Ok(s)) if !s.is_empty() => s,
                    Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result"}).to_string(),
                    Ok(Err(e)) => serde_json::json!({"ok": false, "error": describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(&e)}).to_string(),
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
    HEARTBEAT_DAEMON_BOOT_TS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
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
    let mut last_plugin_update_poll = seed_poll_timer_from_persisted_ts(&persisted_plugin_poll_ts_path());
    let persisted_plugin_poll_ts_at_boot = read_persisted_poll_ts(&persisted_plugin_poll_ts_path());
    if persisted_plugin_poll_ts_at_boot > 0 {
        HEARTBEAT_LAST_PLUGIN_POLL_TS.store(persisted_plugin_poll_ts_at_boot, std::sync::atomic::Ordering::Relaxed);
    }

    let runner_update_poll_interval = daemon_cfg.runner_update_poll_interval();
    let mut last_runner_update_poll = seed_poll_timer_from_persisted_ts(&persisted_runner_poll_ts_path());
    let persisted_runner_poll_ts_at_boot = read_persisted_poll_ts(&persisted_runner_poll_ts_path());
    if persisted_runner_poll_ts_at_boot > 0 {
        HEARTBEAT_LAST_RUNNER_POLL_TS.store(persisted_runner_poll_ts_at_boot, std::sync::atomic::Ordering::Relaxed);
    }
    let mut pending_self_update: Option<(PathBuf, String)> = None;

    let mut last_instruction_source_sync: HashMap<PathBuf, Instant> = HashMap::new();

    let mut last_browser_orphan_sweep = Instant::now()
        .checked_sub(Duration::from_millis(5 * 60 * 1000))
        .unwrap_or_else(Instant::now);

    let _heartbeat_ticker = spawn_heartbeat_ticker(heartbeat_interval);
    write_daemon_heartbeat(0, 0);

    const PROJECT_HEARTBEAT_TICK_INTERVAL_MS: u64 = 3_000;
    let _project_heartbeat_ticker = spawn_project_heartbeat_ticker(Duration::from_millis(PROJECT_HEARTBEAT_TICK_INTERVAL_MS));

    loop {
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            for root in &known_roots {
                sweep_orphaned_claims(root);
            }
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting before serving further work");
            return Ok(());
        }

        if first_registry_poll_pending || last_registry_poll.elapsed() >= registry_poll_interval {
            let sweep_orphans_left_by_whatever_daemon_died_before_answering = first_registry_poll_pending;
            first_registry_poll_pending = false;
            last_registry_poll = Instant::now();
            known_roots = read_registry();
            set_known_project_roots(&known_roots);
            if sweep_orphans_left_by_whatever_daemon_died_before_answering {
                for root in &known_roots {
                    sweep_orphaned_claims(root);
                }
            }
        }

        const BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS: u64 = 5 * 60 * 1000;
        if last_browser_orphan_sweep.elapsed() >= Duration::from_millis(BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS) {
            last_browser_orphan_sweep = Instant::now();
            agentplug_host::reap_idle_sessions_and_os_orphans_across_every_known_project_root(&known_roots);
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
                let thread_root = root.clone();
                std::thread::spawn(move || {
                    if let Err(e) = sync_instruction_source_if_configured(&thread_root) {
                        eprintln!("[agentplug daemon] instruction source-repo sync failed for {}: {e:#}", thread_root.display());
                    }
                });
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

        let forced_refresh_request = take_forced_plugin_refresh_request();
        if last_plugin_update_poll.elapsed() >= plugin_update_poll_interval || forced_refresh_request.is_some() {
            last_plugin_update_poll = Instant::now();
            let poll_ts = now_ms();
            HEARTBEAT_LAST_PLUGIN_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
            write_persisted_poll_ts(&persisted_plugin_poll_ts_path(), poll_ts);
            let targets: Vec<String> = match &forced_refresh_request {
                Some(Some(name)) => vec![name.clone()],
                _ => plugin_modules.modules.keys().cloned().collect(),
            };
            let mut cycle_errors: Vec<String> = Vec::new();
            for plugin_name in targets {
                match crate::download::refresh_plugin_if_stale(&plugin_name) {
                    Ok(Some(new_version)) => {
                        eprintln!(
                            "[agentplug daemon] downloaded+verified plugin {plugin_name} update to {new_version} -- the next tick's get_or_compile content-hash check evicts and recompiles it unconditionally, no idle window required"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let msg = format!("plugin update check for {plugin_name} failed: {e}");
                        eprintln!("[agentplug daemon] {msg}");
                        cycle_errors.push(msg);
                    }
                }
            }
            record_plugin_poll_error(if cycle_errors.is_empty() { None } else { Some(cycle_errors.join("; ")) });
        }

        if last_runner_update_poll.elapsed() >= runner_update_poll_interval || take_forced_runner_refresh_request() {
            last_runner_update_poll = Instant::now();
            let poll_ts = now_ms();
            HEARTBEAT_LAST_RUNNER_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
            write_persisted_poll_ts(&persisted_runner_poll_ts_path(), poll_ts);
            match crate::download::stage_runner_self_update() {
                Ok(Some((staged, version))) => {
                    eprintln!("[agentplug daemon] staged self-update to {version} at {}", staged.display());
                    pending_self_update = Some((staged, version));
                    record_runner_poll_error(None);
                }
                Ok(None) => record_runner_poll_error(None),
                Err(e) => {
                    let msg = format!("runner self-update check failed: {e}");
                    eprintln!("[agentplug daemon] {msg}");
                    record_runner_poll_error(Some(msg));
                }
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

        if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&daemon_cfg) {
            let mut released: Vec<&str> = Vec::new();
            for plugin_name in ["bert", "treesitter", "libsql"] {
                if agentplug_host::release_shared_plugin(plugin_name) {
                    released.push(plugin_name);
                }
            }
            agentplug_host::reset_shared_dispatch_count();
            last_shared_release = Instant::now();
            if !released.is_empty() {
                eprintln!(
                    "[agentplug daemon] released shared Stores [{}] under {reason} -- wasm linear memory only grows, so the retained embed peak is only reclaimable by dropping the Store; the compiled Module stays cached in the Engine, so the next call re-instantiates cheaply",
                    released.join(", ")
                );
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
