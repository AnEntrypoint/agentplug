use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use wasmtime::{Engine, Module, Trap};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, DispatchHandle, GmFairnessGuard, ProjectPlugins, ToolDispatchGuard};

use crate::download::{ensure_plugin_installed, installed_plugin_version, installed_runner_version, is_recognized_release_semver, record_runner_version};

fn registry_path() -> PathBuf {
    install_dir().join("daemon-registry.txt")
}

const GM_SPOOL_VERBS: &[&str] = &[
    "instruction", "transition", "phase-status", "prd-add", "prd-list", "prd-resolve",
    "prd-status", "mutable-add", "mutable-list", "mutable-resolve", "fs_read", "fs_write",
    "fs_readdir", "fs_stat", "scan_deps", "fetch", "env_get", "kv_get", "kv_put",
    "kv_query", "exec_js", "lang", "serp", "browser", "cdp", "health",
    "config_resolve", "config-sync-now", "dataflow_resolve", "sql_open", "sql_close",
    "sql_list_dbs", "sql_exec", "sql_query", "sql_smoke", "sql_serialize",
    "sql_deserialize", "cache_get", "cache_put", "cache_invalidate", "cache_stats",
    "codeinsight_index", "codesearch", "callers", "callees", "impact", "memorize",
    "memorize-prune", "memorize-vacuum", "memorize-retention", "recall",
    "tencentdb-compat-probe", "tencentdb-memory-import", "python", "bash", "powershell",
    "ssh", "go", "rust", "c", "cpp", "java", "deno", "status", "wait", "close",
    "filter", "git_status", "branch_status", "git_push", "git_add", "git_commit",
    "git_finalize", "git_log", "git_diff", "git_show", "git_fetch", "git_pull",
    "ci-status", "git_branch", "git_checkout", "git_merge", "git_merge_abort",
    "git_branch_delete", "git_rm", "git_revert", "git_reset", "git_poll", "forget",
    "discipline",
];

fn provision_gm_spool_verb_dirs(cwd: &Path) -> anyhow::Result<()> {
    let in_dir = cwd.join(".gm").join("exec-spool").join("in");
    for verb in GM_SPOOL_VERBS {
        fs::create_dir_all(in_dir.join(verb))?;
    }
    Ok(())
}

fn cwd_is_inside_a_spool_tree(cwd: &Path) -> bool {
    cwd.components().any(|c| c.as_os_str() == ".gm")
        && cwd.to_string_lossy().replace('\\', "/").contains("/.gm/exec-spool")
}

pub fn register_project(cwd: &Path) -> anyhow::Result<()> {
    if cwd_is_inside_a_spool_tree(cwd) {
        anyhow::bail!(
            "refusing to register {} as a project root -- its own path is already inside a .gm/exec-spool tree, which means this is spool runtime state (in/out/status files), not a genuine project directory. Launch the spool from the actual project root instead.",
            cwd.display()
        );
    }
    provision_gm_spool_verb_dirs(cwd)?;
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

fn host_available_parallelism() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(4)
}

#[derive(serde::Deserialize, Clone)]
struct DaemonConfig {
    #[serde(default)]
    registry_poll_interval_secs: Option<u64>,
    #[serde(default)]
    heartbeat_interval_secs: Option<u64>,
    #[serde(default)]
    plugin_update_poll_interval_secs: Option<u64>,
    #[serde(default)]
    plugin_update_poll_interval_secs_by_name: std::collections::HashMap<String, u64>,
    #[serde(default)]
    runner_update_poll_interval_secs: Option<u64>,
    #[serde(default)]
    instruction_source_poll_interval_secs: Option<u64>,
    #[serde(default)]
    gm_concurrency: Option<usize>,
    #[serde(default)]
    side_plugin_concurrency: Option<usize>,
    #[serde(default)]
    gm_pool_size: Option<usize>,
    #[serde(default)]
    shared_store_recycle_private_mb: Option<u64>,
    #[serde(default)]
    shared_store_recycle_dispatches: Option<u64>,
    #[serde(default)]
    project_idle_evict_secs: Option<u64>,
    #[serde(default)]
    shared_plugin_release_idle_secs: Option<u64>,
}

const DAEMON_CONFIG_EXAMPLE: &str = r#"{
  "registry_poll_interval_secs": 5,
  "heartbeat_interval_secs": 10,
  "plugin_update_poll_interval_secs": 600,
  "plugin_update_poll_interval_secs_by_name": {},
  "runner_update_poll_interval_secs": 3600,
  "instruction_source_poll_interval_secs": 600
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
                plugin_update_poll_interval_secs_by_name: std::collections::HashMap::new(),
                runner_update_poll_interval_secs: None,
                instruction_source_poll_interval_secs: None,
                gm_concurrency: None,
                side_plugin_concurrency: None,
                gm_pool_size: None,
                shared_store_recycle_private_mb: None,
                shared_store_recycle_dispatches: None,
                project_idle_evict_secs: None,
                shared_plugin_release_idle_secs: None,
            }
    }
    fn registry_poll_interval(&self) -> Duration { Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5)) }
    fn heartbeat_interval(&self) -> Duration { Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10)) }
    fn plugin_update_poll_interval(&self) -> Duration { Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600)) }
    fn plugin_update_poll_interval_for(&self, plugin_name: &str) -> Duration {
        match self.plugin_update_poll_interval_secs_by_name.get(plugin_name) {
            Some(secs) => Duration::from_secs(*secs),
            None => self.plugin_update_poll_interval(),
        }
    }
    fn runner_update_poll_interval(&self) -> Duration { Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(3600)) }
    fn instruction_source_poll_interval(&self) -> Duration { Duration::from_secs(self.instruction_source_poll_interval_secs.unwrap_or(600)) }
    fn max_concurrent_projects(&self) -> usize { 4 }
    fn gm_concurrency(&self) -> usize { self.gm_concurrency.unwrap_or_else(|| self.max_concurrent_projects()).max(1) }
    fn gm_pool_size(&self) -> usize {
        self.gm_pool_size
            .unwrap_or(4)
            .min(host_available_parallelism())
            .min(4)
            .max(1)
    }

    fn gm_pool_capacity_reason(&self) -> String {
        let capacity = self.gm_pool_size();
        if capacity == 4 {
            "four processors admitted by host capacity".to_string()
        } else {
            format!("limited to {capacity} processor(s) by configured or host capacity")
        }
    }
    fn side_plugin_concurrency(&self) -> usize {
        self.side_plugin_concurrency.unwrap_or(1).max(1)
    }
    fn shared_store_recycle_private_bytes(&self) -> u64 {
        const DEFAULT_MB: u64 = 1600;
        self.shared_store_recycle_private_mb.unwrap_or(DEFAULT_MB).max(256) * 1024 * 1024
    }
    fn shared_store_recycle_dispatches(&self) -> u64 {
        let default = 500u64.saturating_mul(self.gm_concurrency() as u64).max(100);
        self.shared_store_recycle_dispatches.unwrap_or(default).max(1)
    }
    fn project_idle_evict_ms(&self) -> u64 {
        const DEFAULT_SECS: u64 = 30 * 60;
        self.project_idle_evict_secs.unwrap_or(DEFAULT_SECS).max(60) * 1000
    }
    fn shared_plugin_release_idle_ms(&self) -> u64 {
        const DEFAULT_SECS: u64 = 30 * 60;
        const MIN_SECS: u64 = 5 * 60;
        self.shared_plugin_release_idle_secs.unwrap_or(DEFAULT_SECS).max(MIN_SECS) * 1000
    }
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

    let recheck_pid = read_owner_pid();
    let recheck_still_stale = recheck_pid.map(|p| !pid_is_alive(p)).unwrap_or(true)
        || fs::read_to_string(daemon_status_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .map(|v| {
                let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
                now_ms().saturating_sub(ts) >= DAEMON_STALE_MS
            })
            .unwrap_or(true);
    if !recheck_still_stale {
        return recheck_pid == Some(my_pid);
    }
    if let Some(other_pid) = recheck_pid {
        if other_pid != my_pid && other_pid < my_pid && pid_is_alive(other_pid) {
            return false;
        }
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

pub fn shared_daemon_owner_that_would_refuse_this_process() -> Option<u64> {
    let owner_pid = read_owner_pid()?;
    if owner_pid == std::process::id() as u64 {
        return None;
    }
    let heartbeat_fresh = fs::read_to_string(daemon_status_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|v| {
            let pid = v.get("pid").and_then(|p| p.as_u64());
            let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
            now_ms().saturating_sub(ts) < DAEMON_STALE_MS && pid == Some(owner_pid)
        })
        .unwrap_or(false);
    if heartbeat_fresh && pid_is_alive(owner_pid) {
        Some(owner_pid)
    } else {
        None
    }
}

pub fn live_foreign_daemon_owner_pid() -> Option<u64> {
    let pid = read_owner_pid()?;
    if pid == std::process::id() as u64 {
        return None;
    }
    if pid_is_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn daemon_spawn_backoff_path() -> PathBuf {
    install_dir().join("daemon-spawn-backoff.json")
}

const WASTED_DAEMON_START_BACKOFF_BASE_MS: u64 = 5_000;

const WASTED_DAEMON_START_BACKOFF_CEILING_MS: u64 = 120_000;

fn read_wasted_daemon_start_backoff() -> (u64, u32) {
    fs::read_to_string(daemon_spawn_backoff_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|v| {
            (
                v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0),
                v.get("wasted_starts").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
            )
        })
        .unwrap_or((0, 0))
}

fn record_wasted_daemon_start() {
    let (_, wasted_starts) = read_wasted_daemon_start_backoff();
    let path = daemon_spawn_backoff_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "ts": now_ms(), "wasted_starts": wasted_starts.saturating_add(1) });
    let _ = fs::write(&path, payload.to_string());
}

fn clear_wasted_daemon_start_backoff() {
    let _ = fs::remove_file(daemon_spawn_backoff_path());
}

pub fn daemon_spawn_backoff_remaining_ms_if_active() -> Option<u64> {
    let remaining = wasted_daemon_start_backoff_remaining_ms();
    if remaining > 0 {
        Some(remaining)
    } else {
        None
    }
}

fn wasted_daemon_start_backoff_remaining_ms() -> u64 {
    if live_foreign_daemon_owner_pid().is_none() {
        return 0;
    }
    let (ts, wasted_starts) = read_wasted_daemon_start_backoff();
    if ts == 0 || wasted_starts == 0 {
        return 0;
    }
    let window = WASTED_DAEMON_START_BACKOFF_BASE_MS
        .saturating_mul(1u64 << wasted_starts.min(5))
        .min(WASTED_DAEMON_START_BACKOFF_CEILING_MS);
    window.saturating_sub(now_ms().saturating_sub(ts))
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
    let backoff_remaining = wasted_daemon_start_backoff_remaining_ms();
    if backoff_remaining > 0 {
        eprintln!(
            "[agentplug] not spawning a daemon for another {backoff_remaining}ms -- a recent start already lost the ownership claim to live pid {:?}, which is alive but publishing a stale heartbeat; spawning again at this rate only burns processes",
            read_owner_pid()
        );
        return Ok(false);
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
    let result = match spawn_result {
        Ok(()) => {
            let mut fresh = false;
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(200));
                if is_daemon_fresh() {
                    fresh = true;
                    break;
                }
            }
            Ok(fresh)
        }
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&lock_path);
    result
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

fn daemon_log_path() -> PathBuf {
    install_dir().join("daemon.log")
}

const DAEMON_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

fn daemon_log_sink() -> Option<fs::File> {
    let path = daemon_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path).map(|m| m.len() > DAEMON_LOG_MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_extension("log.prev"));
    }
    fs::OpenOptions::new().create(true).append(true).open(&path).ok()
}

fn spawn_detached(exe: &Path, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    match daemon_log_sink() {
        Some(log) => cmd.stderr(std::process::Stdio::from(log)),
        None => cmd.stderr(std::process::Stdio::null()),
    };
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
    // Drain both pipes on their own threads before waiting, so a child whose output exceeds the
    // ~64 KB OS pipe buffer cannot deadlock (it keeps writing while we read); wait_timeout still
    // bounds a genuinely stuck network op.
    let out_reader = child.stdout.take().map(|mut o| std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut o, &mut buf);
        buf
    }));
    let err_reader = child.stderr.take().map(|mut e| std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut e, &mut buf);
        buf
    }));
    match child.wait_timeout(Duration::from_millis(timeout_ms))? {
        Some(status) => {
            let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
            let stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
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

fn staged_binary_self_check(staged_exe: &Path, expected_version: &str) -> bool {
    let mut cmd = std::process::Command::new(staged_exe);
    cmd.arg("--version");
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains(expected_version) {
                true
            } else {
                eprintln!(
                    "[agentplug daemon] staged binary {} --version printed {:?}, expected to contain {expected_version} -- refusing handoff",
                    staged_exe.display(), text.trim()
                );
                false
            }
        }
        Ok(out) => {
            eprintln!(
                "[agentplug daemon] staged binary {} --version exited with {} -- refusing handoff",
                staged_exe.display(), out.status
            );
            false
        }
        Err(e) => {
            eprintln!("[agentplug daemon] staged binary {} --version failed to spawn: {e} -- refusing handoff", staged_exe.display());
            false
        }
    }
}

fn attempt_self_update_handoff(staged_exe: &Path, version: &str) -> bool {
    if !staged_binary_self_check(staged_exe, version) {
        let _ = fs::remove_file(staged_exe);
        record_handoff_failure(version, format!("staged_binary_self_check failed for {version}, staged exe removed"));
        return false;
    }
    let ready_path = takeover_ready_path();
    let _ = fs::remove_file(&ready_path);
    if let Err(e) = spawn_detached(staged_exe, &["takeover", version]) {
        record_handoff_failure(version, format!("spawn_detached of staged {version} failed: {e}"));
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(raw) = fs::read_to_string(&ready_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v.get("version").and_then(|x| x.as_str()) == Some(version) {
                    eprintln!("[agentplug daemon] new version {version} confirmed ready -- releasing ownership for handoff");
                    record_handoff_attempt(None);
                    clear_handoff_escalation(version);
                    release_ownership_for_handoff();
                    return true;
                }
            }
        }
    }
    eprintln!("[agentplug daemon] self-update to {version} did not confirm ready in time -- staying on current version, will retry next poll");
    record_handoff_failure(version, format!("staged {version} did not write a matching readiness marker within 10s"));
    false
}

fn release_ownership_for_handoff() {
    let my_pid = std::process::id() as u64;
    if read_owner_pid() == Some(my_pid) {
        let _ = fs::remove_file(daemon_owner_path());
    }
}

fn promote_staged_exe_to_canonical(version: &str) -> bool {
    let Some(canonical) = canonical_runner_exe_path() else { return false };
    let Ok(staged) = std::env::current_exe() else { return false };
    if staged == canonical {
        return false;
    }
    let prev = canonical.with_extension(
        canonical.extension().map(|e| format!("{}.prev", e.to_string_lossy())).unwrap_or_else(|| "prev".to_string()),
    );
    if canonical.exists() {
        if let Err(e) = fs::rename(&canonical, &prev) {
            eprintln!(
                "[agentplug daemon] takeover: could not back up canonical exe {} to {} before promoting {version}: {e} -- leaving canonical path stale, daemon keeps running from staged copy",
                canonical.display(), prev.display()
            );
            return false;
        }
    }
    match fs::copy(&staged, &canonical) {
        Ok(_) => {
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&canonical) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    let _ = fs::set_permissions(&canonical, perms);
                }
            }
            record_completed_runner_swap(version);
            eprintln!("[agentplug daemon] takeover: promoted {version} onto canonical exe path {} (previous version kept at {})", canonical.display(), prev.display());
            true
        }
        Err(e) => {
            eprintln!(
                "[agentplug daemon] takeover: failed to copy staged exe onto canonical path {}: {e} -- restoring previous version at canonical path",
                canonical.display()
            );
            if prev.exists() {
                let _ = fs::rename(&prev, &canonical);
            }
            false
        }
    }
}

fn reexec_from_canonical_and_exit(canonical: &std::path::Path) -> ! {
    eprintln!("[agentplug daemon] takeover: re-execing from canonical path {} to release lock on staged exe", canonical.display());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = std::process::Command::new(canonical);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    match daemon_log_sink() {
        Some(log) => { cmd.stderr(std::process::Stdio::from(log)); }
        None => { cmd.stderr(std::process::Stdio::null()); }
    };
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        Ok(_) => {
            eprintln!("[agentplug daemon] takeover: spawned fresh process from canonical path, exiting stale staged-exe process");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[agentplug daemon] takeover: failed to re-exec from canonical path {}: {e} -- continuing to run from stale staged exe (will retry re-exec on next takeover)", canonical.display());
            std::process::exit(1);
        }
    }
}

pub fn run_takeover(version: &str) -> anyhow::Result<()> {
    eprintln!("[agentplug daemon] takeover: building engine for version {version}");
    let mut plugin_modules = PluginModules::new()?;
    for plugin_name in ["gm", "bert", "libsql", "treesitter", "oxibrowser", "crux"] {
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
            crate::download::clear_all_known_bad_version_markers();
            let promoted = promote_staged_exe_to_canonical(version);
            if promoted {
                if let Some(canonical) = canonical_runner_exe_path() {
                    release_ownership_for_handoff();
                    reexec_from_canonical_and_exit(&canonical);
                }
            }
            eprintln!("[agentplug daemon] takeover: ownership claimed, version recorded, entering normal daemon loop");
            return run_daemon_body(plugin_modules);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    anyhow::bail!("takeover: old daemon never released ownership within the wait window -- aborting, old daemon keeps serving")
}

fn pending_store_swaps_by_plugin() -> serde_json::Map<String, serde_json::Value> {
    ["gm", "bert", "libsql", "treesitter"]
        .iter()
        .filter_map(|name| {
            let hashes = agentplug_host::shared_plugin_swap_pending_hashes(name);
            if hashes.is_empty() { None } else { Some((name.to_string(), serde_json::json!(hashes))) }
        })
        .collect()
}

fn write_daemon_heartbeat(project_count: usize, plugin_module_count: usize) {
    let last_plugin_poll_ts = HEARTBEAT_LAST_PLUGIN_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let last_runner_poll_ts = HEARTBEAT_LAST_RUNNER_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let loaded_content_hashes: HashMap<String, String> =
        loaded_plugin_content_hashes().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let shared_pool_slots: HashMap<String, Vec<agentplug_host::SlotContentSnapshot>> = ["gm", "bert", "libsql", "treesitter"]
        .iter()
        .map(|name| (name.to_string(), agentplug_host::shared_plugin_slot_snapshot_without_blocking(name)))
        .collect();
    let mixed_version_pools: Vec<String> = shared_pool_slots
        .iter()
        .filter(|(_, slots)| {
            slots
                .iter()
                .filter_map(|s| match s {
                    agentplug_host::SlotContentSnapshot::Loaded { content_hash } => Some(content_hash),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1
        })
        .map(|(name, _)| name.clone())
        .collect();
    let shared_pool_slot_hashes: HashMap<String, Vec<serde_json::Value>> = shared_pool_slots
        .into_iter()
        .map(|(name, slots)| {
            let rendered = slots
                .into_iter()
                .map(|slot| match slot {
                    agentplug_host::SlotContentSnapshot::Empty => serde_json::Value::Null,
                    agentplug_host::SlotContentSnapshot::Loaded { content_hash } => serde_json::json!(content_hash),
                    agentplug_host::SlotContentSnapshot::BusyWithDispatchInFlight => serde_json::json!("busy-dispatch-in-flight"),
                })
                .collect();
            (name, rendered)
        })
        .collect();
    let boot_ts = HEARTBEAT_DAEMON_BOOT_TS.load(std::sync::atomic::Ordering::Relaxed);
    let plugin_poll_error = last_plugin_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let runner_poll_error = last_runner_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let staged_runner = refresh_staged_runner_cache();
    let handoff_attempt = last_handoff_attempt().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let plugin_compile_failures: HashMap<String, String> =
        last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).clone();
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
            "plugin_compile_failures": plugin_compile_failures,
            "loaded_plugin_content_sha256": loaded_content_hashes,
            "shared_pool_slot_content_sha256": shared_pool_slot_hashes,
            "mixed_version_pools": mixed_version_pools,
            "pending_store_swaps": pending_store_swaps_by_plugin(),
            "staged_runner_awaiting_handoff": staged_runner.is_some(),
            "staged_runner_since_ts": staged_runner.map(|(since_ts, _)| serde_json::json!(since_ts)).unwrap_or(serde_json::Value::Null),
            "staged_runner_waiting_ms": staged_runner.map(|(since_ts, _)| serde_json::json!(now_ms().saturating_sub(since_ts))).unwrap_or(serde_json::Value::Null),
            "last_handoff_attempt_ts": handoff_attempt.as_ref().map(|(ts, _)| serde_json::json!(ts)).unwrap_or(serde_json::Value::Null),
            "last_handoff_error": handoff_attempt.as_ref().and_then(|(_, err)| err.clone()),
            "last_completed_runner_swap": read_last_completed_runner_swap().unwrap_or(serde_json::Value::Null),
        })
        .to_string(),
    );
}

fn last_completed_runner_swap_path() -> PathBuf {
    install_dir().join("last-completed-runner-swap.json")
}

fn record_completed_runner_swap(version: &str) {
    let _ = fs::write(
        last_completed_runner_swap_path(),
        serde_json::json!({ "version": version, "swapped_at_ts": now_ms() }).to_string(),
    );
}

fn read_last_completed_runner_swap() -> Option<serde_json::Value> {
    let text = fs::read_to_string(last_completed_runner_swap_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn canonical_runner_exe_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    while path.extension().map(|e| e.eq_ignore_ascii_case("new")).unwrap_or(false) {
        path = path.with_extension("");
    }
    Some(path)
}

fn staged_matches_running(canonical: &Path, staged: &Path) -> bool {
    let Ok(running_meta) = fs::metadata(canonical) else { return false };
    let Ok(staged_meta) = fs::metadata(staged) else { return false };
    if running_meta.len() != staged_meta.len() {
        return false;
    }
    let Ok(running_bytes) = fs::read(canonical) else { return false };
    let Ok(staged_bytes) = fs::read(staged) else { return false };
    running_bytes == staged_bytes
}

fn staged_runner_awaiting_handoff() -> Option<(u64, u64)> {
    let canonical = canonical_runner_exe_path()?;
    let staged = canonical.with_extension(
        canonical.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string()),
    );
    if staged_matches_running(&canonical, &staged) {
        let _ = fs::remove_file(&staged);
        let _ = fs::remove_file(takeover_ready_path());
        return None;
    }
    let meta = fs::metadata(&staged).ok()?;
    let staged_at_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((staged_at_ms, meta.len()))
}

fn staged_runner_cache() -> &'static Mutex<Option<(u64, u64)>> {
    static SLOT: OnceLock<Mutex<Option<(u64, u64)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn refresh_staged_runner_cache() -> Option<(u64, u64)> {
    let value = staged_runner_awaiting_handoff();
    *staged_runner_cache().lock().unwrap_or_else(|e| e.into_inner()) = value;
    value
}

fn cached_staged_runner() -> Option<(u64, u64)> {
    *staged_runner_cache().lock().unwrap_or_else(|e| e.into_inner())
}

fn write_project_heartbeat(spool_dir: &Path, busy_until: Option<u64>) {
    write_project_heartbeat_with_queue_info(spool_dir, busy_until, None);
}

fn write_project_heartbeat_with_queue_info(spool_dir: &Path, busy_until: Option<u64>, queue_info: Option<(usize, usize)>) {
    let status_path = spool_dir.join(".status.json");
    let mut payload = match fs::read_to_string(&status_path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
        Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => serde_json::json!({}),
    };
    payload["pid"] = serde_json::json!(std::process::id());
    payload["ts"] = serde_json::json!(now_ms());
    payload["daemon"] = serde_json::json!(true);
    payload["shared_process"] = serde_json::json!(true);
    payload["runtime"] = serde_json::json!("agentplug");
    if let Some(busy_until) = busy_until {
        payload["busy_until"] = serde_json::json!(busy_until);
    } else {
        payload.as_object_mut().map(|m| m.remove("busy_until"));
    }
    if let Some((position, total)) = queue_info {
        payload["queue_position"] = serde_json::json!(position);
        payload["queue_depth"] = serde_json::json!(total);
    }
    let (queued_steps, claimed_steps) = spool_step_counts(spool_dir);
    payload["queued_step_count"] = serde_json::json!(queued_steps);
    payload["claimed_step_count"] = serde_json::json!(claimed_steps);
    payload["gm_processor_capacity"] = serde_json::json!(GM_PROCESSOR_CAPACITY.load(std::sync::atomic::Ordering::Relaxed));
    payload["gm_processor_capacity_reason"] = serde_json::json!(gm_processor_capacity_reason().lock().unwrap_or_else(|e| e.into_inner()).clone());
    payload["shared_store_recycle_limit_mb"] = serde_json::json!(SHARED_STORE_RECYCLE_LIMIT_MB.load(std::sync::atomic::Ordering::Relaxed));
    payload["tool_serialization"] = serde_json::json!("fifo per plugin and verb");
    payload["queue_wait_ms"] = serde_json::json!(last_measured_dispatch_queue_wait_ms());
    if let Some((staged_at_ms, _len)) = cached_staged_runner() {
        payload["runner_update_in_progress"] = serde_json::json!(true);
        payload["runner_update_waiting_ms"] = serde_json::json!(now_ms().saturating_sub(staged_at_ms));
    }
    let failures = last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if failures.is_empty() {
        payload.as_object_mut().map(|m| m.remove("plugin_compile_failures"));
    } else {
        payload["plugin_compile_failures"] = serde_json::json!(failures);
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

pub fn read_known_project_roots() -> Vec<PathBuf> {
    known_project_roots().lock().unwrap_or_else(|e| e.into_inner()).clone()
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
            write_project_heartbeat(&spool_dir, busy_until_for_project_ticker(&spool_dir));
        }
    })
}

fn read_status_busy_until_if_future(spool_dir: &Path) -> Option<u64> {
    let text = fs::read_to_string(spool_dir.join(".status.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let busy_until = value.get("busy_until")?.as_u64()?;
    (busy_until > now_ms()).then_some(busy_until)
}

const TICKER_BUSY_UNTIL_EXTEND_MS: u64 = 60_000;

fn busy_until_for_project_ticker(spool_dir: &Path) -> Option<u64> {
    if spool_has_queued_work(spool_dir) {
        return Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS);
    }
    read_status_busy_until_if_future(spool_dir)
}

fn spool_has_queued_work(spool_dir: &Path) -> bool {
    let in_dir = spool_dir.join("in");
    let Ok(verbs) = fs::read_dir(&in_dir) else { return false };
    for verb_entry in verbs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            let name = file_entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".inflight") || name.ends_with(".txt") {
                return true;
            }
        }
    }
    false
}

fn spool_step_counts(spool_dir: &Path) -> (usize, usize) {
    let mut queued = 0usize;
    let mut claimed = 0usize;
    let in_dir = spool_dir.join("in");
    let Ok(verbs) = fs::read_dir(in_dir) else { return (queued, claimed) };
    for verb_entry in verbs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            let name = file_entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".txt") {
                queued += 1;
            } else if name.ends_with(".inflight") {
                claimed += 1;
            }
        }
    }
    (queued, claimed)
}

static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_LAST_PLUGIN_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_LAST_RUNNER_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_DAEMON_BOOT_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GM_PROCESSOR_CAPACITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
static SHARED_STORE_RECYCLE_LIMIT_MB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn gm_processor_capacity_reason() -> &'static Mutex<String> {
    static SLOT: OnceLock<Mutex<String>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new("daemon configuration not loaded".to_string()))
}

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

type HandoffAttempt = (u64, Option<String>);

fn last_handoff_attempt() -> &'static Mutex<Option<HandoffAttempt>> {
    static SLOT: OnceLock<Mutex<Option<HandoffAttempt>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_handoff_attempt(error: Option<String>) {
    *last_handoff_attempt().lock().unwrap_or_else(|e| e.into_inner()) = Some((now_ms(), error));
}

fn consecutive_handoff_failures() -> &'static Mutex<(String, u32)> {
    static SLOT: OnceLock<Mutex<(String, u32)>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new((String::new(), 0)))
}

const HANDOFF_ESCALATION_THRESHOLD: u32 = 3;

fn runner_update_escalation_path() -> PathBuf {
    install_dir().join("runner-update-escalation.json")
}

pub(crate) fn patch_update_available_from_escalation(plugin: &str, verb: &str, response: String) -> String {
    if plugin != "gm" || verb != "instruction" {
        return response;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&response) else { return response };
    let Some(obj) = value.as_object_mut() else { return response };
    if !matches!(obj.get("update_available"), Some(serde_json::Value::Null) | None) {
        return response;
    }
    let Ok(marker_raw) = fs::read_to_string(runner_update_escalation_path()) else { return response };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&marker_raw) else { return response };
    obj.insert("update_available".to_string(), marker);
    value.to_string()
}

fn record_handoff_failure(version: &str, reason: String) {
    record_handoff_attempt(Some(reason.clone()));
    let mut slot = consecutive_handoff_failures().lock().unwrap_or_else(|e| e.into_inner());
    if slot.0 != version {
        *slot = (version.to_string(), 1);
    } else {
        slot.1 += 1;
    }
    if slot.1 < HANDOFF_ESCALATION_THRESHOLD {
        return;
    }
    let command = if cfg!(windows) {
        r#"irm https://raw.githubusercontent.com/AnEntrypoint/gm/main/install.ps1 | iex"#
    } else {
        "curl -fsSL https://raw.githubusercontent.com/AnEntrypoint/gm/main/install.sh | sh -s -- spool"
    };
    let marker = serde_json::json!({
        "version": version,
        "consecutive_failures": slot.1,
        "reason": reason,
        "command": command,
        "since_ts": now_ms(),
    });
    if let Some(parent) = runner_update_escalation_path().parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(runner_update_escalation_path(), marker.to_string());
}

fn clear_handoff_escalation(version: &str) {
    let mut slot = consecutive_handoff_failures().lock().unwrap_or_else(|e| e.into_inner());
    if slot.0 == version {
        *slot = (String::new(), 0);
    }
    let _ = fs::remove_file(runner_update_escalation_path());
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

static LAST_PLUGIN_COMPILE_FAILURE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn last_plugin_compile_failure() -> &'static Mutex<HashMap<String, String>> {
    LAST_PLUGIN_COMPILE_FAILURE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_plugin_compile_failure(plugin_name: &str, reason: String) {
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).insert(plugin_name.to_string(), reason);
    plugin_compile_backoff_until().lock().unwrap_or_else(|e| e.into_inner()).insert(plugin_name.to_string(), Instant::now() + PLUGIN_COMPILE_RETRY_BACKOFF);
}

fn clear_plugin_compile_failure(plugin_name: &str) {
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).remove(plugin_name);
    plugin_compile_backoff_until().lock().unwrap_or_else(|e| e.into_inner()).remove(plugin_name);
}

const PLUGIN_COMPILE_RETRY_BACKOFF: Duration = Duration::from_secs(60);

static PLUGIN_COMPILE_BACKOFF_UNTIL: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn plugin_compile_backoff_until() -> &'static Mutex<HashMap<String, Instant>> {
    PLUGIN_COMPILE_BACKOFF_UNTIL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn plugin_compile_in_backoff(plugin_name: &str) -> bool {
    plugin_compile_backoff_until()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(plugin_name)
        .is_some_and(|until| Instant::now() < *until)
}

fn read_plugin_compile_failure(plugin_name: &str) -> Option<String> {
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).get(plugin_name).cloned()
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
            eprintln!(
                "[agentplug daemon] heartbeat ticker: authority lost to another daemon -- the main loop only checks this flag between dispatch batches, which can be blocked indefinitely by in-flight work, so exiting the process directly here instead of merely signaling"
            );
            HEARTBEAT_AUTHORITY_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
            agentplug_host::close_all_sessions();
            let requeued = hand_claims_to_live_successor("heartbeat-authority-holder");
            eprintln!("[agentplug daemon] heartbeat ticker: re-queued {requeued} in-flight claim(s) for the daemon that holds authority -- exiting without orphaning them");
            std::process::exit(0);
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
    last_hash_check_stat: HashMap<String, (std::time::SystemTime, u64)>,
}

fn wasm_file_content_hash(wasm_path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(wasm_path)?;
    Ok(crate::download::sha256_hex(&bytes))
}

fn wasm_file_stat(wasm_path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = fs::metadata(wasm_path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((mtime, meta.len()))
}

impl PluginModules {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            engine: build_engine()?,
            modules: HashMap::new(),
            loaded_content_hash: HashMap::new(),
            last_hash_check_stat: HashMap::new(),
        })
    }

    fn get_or_compile(&mut self, plugin_name: &str) -> anyhow::Result<()> {
        let wasm_path = ensure_plugin_installed(plugin_name, None)?;
        let current_stat = wasm_file_stat(&wasm_path);
        let stat_unchanged = current_stat.is_some()
            && current_stat == self.last_hash_check_stat.get(plugin_name).copied();
        if stat_unchanged && self.modules.contains_key(plugin_name) {
            return Ok(());
        }
        let on_disk_hash = wasm_file_content_hash(&wasm_path)?;
        if let Some(stat) = current_stat {
            self.last_hash_check_stat.insert(plugin_name.to_string(), stat);
        }
        let old_loaded_hash = self.loaded_content_hash.get(plugin_name).cloned();
        let stale = old_loaded_hash.as_deref().is_some_and(|loaded_hash| loaded_hash != on_disk_hash);
        if stale {
            let old_hash = old_loaded_hash.unwrap_or_default();
            let (evicted_now, deferred) = agentplug_host::request_shared_store_swap(plugin_name, &old_hash);
            eprintln!(
                "[agentplug daemon] {plugin_name}.wasm content hash changed on disk since it was last compiled -- evicting the stale in-process module and draining the shared Stores using it ({evicted_now} slot(s) evicted now, {deferred} still in-flight and finishing on the old Store; their slots evict on completion), forcing a recompile from the current bytes"
            );
            self.modules.remove(plugin_name);
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
                .insert(plugin_name.to_string(), on_disk_hash.clone());
            agentplug_host::note_shared_plugin_bytes_current(plugin_name, &on_disk_hash);
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

pub(crate) type InFlightKey = (PathBuf, String, String);

pub(crate) struct InFlightHandle {
    pub(crate) detach: Arc<std::sync::atomic::AtomicBool>,
}

static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

pub(crate) fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn project_has_in_flight_step(root: &Path) -> bool {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .any(|(active_root, _, _)| active_root == root)
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
            let out_path = root.join(".gm").join("exec-spool").join("out").join(format!("{}-{}.json", req.verb, req.task));
            if out_path.exists() {
                serde_json::json!({"ok": false, "error": "already_completed", "verb": req.verb, "task": req.task}).to_string()
            } else {
                serde_json::json!({"ok": false, "error": "unknown_task", "reason": "no in-flight dispatch and no out/ file found for this verb+task -- this task id was never dispatched, or its verb never matched", "verb": req.verb, "task": req.task}).to_string()
            }
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

pub fn shared_daemon_is_serving() -> bool {
    is_daemon_fresh()
}

const FOREIGN_SWEEPER_STALE_MS: u64 = 120_000;

pub fn live_foreign_spool_sweeper(spool_dir: &Path) -> Option<u64> {
    let status = fs::read_to_string(spool_dir.join(".status.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&status).ok()?;
    let pid = value.get("pid").and_then(|p| p.as_u64())?;
    if pid == std::process::id() as u64 {
        return None;
    }
    let ts = value.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    if now_ms().saturating_sub(ts) >= FOREIGN_SWEEPER_STALE_MS {
        return None;
    }
    if !pid_is_alive(pid) {
        return None;
    }
    Some(pid)
}

const SPOOL_WRITE_SETTLE_MS: u64 = 200;

fn spool_in_file_write_has_settled(txt_path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(txt_path) else { return false };
    if metadata.len() == 0 {
        return false;
    }
    metadata
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|age| age.as_millis() as u64 >= SPOOL_WRITE_SETTLE_MS)
        .unwrap_or(true)
}

pub fn claim_spool_request_in_place(txt_path: &Path) -> Option<PathBuf> {
    if !spool_in_file_write_has_settled(txt_path) {
        return None;
    }
    let claim_path = txt_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
    fs::rename(txt_path, &claim_path).ok().map(|_| claim_path)
}

pub fn write_spool_out(out_dir: &Path, out_name: &str, out_body: &str) {
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

fn queued_request_path(in_dir: &Path, verb: &str, task: &str) -> PathBuf {
    in_dir.join(verb).join(format!("{task}.txt"))
}

fn project_in_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("exec-spool").join("in")
}

type AbandonedClaim = (PathBuf, String, String);

fn requeue_claim(in_dir: &Path, verb: &str, task: &str) -> bool {
    let claim = inflight_claim_path(in_dir, verb, task);
    let queued = queued_request_path(in_dir, verb, task);
    if queued.exists() {
        let _ = fs::remove_file(&claim);
        return true;
    }
    fs::rename(&claim, &queued).is_ok()
}

fn snapshot_in_flight_claims() -> Vec<AbandonedClaim> {
    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
}

fn requeue_claims_for_live_successor(claims: &[AbandonedClaim]) -> usize {
    claims.iter().filter(|(root, verb, task)| requeue_claim(&project_in_dir(root), verb, task)).count()
}

fn hand_claims_to_live_successor(successor: &str) -> usize {
    let claims = snapshot_in_flight_claims();
    write_handoff_inherited_claims(successor, &claims);
    requeue_claims_for_live_successor(&claims)
}

fn handoff_inherited_claims_path() -> PathBuf {
    install_dir().join("handoff-inherited-claims.json")
}

const HANDOFF_INHERITED_CLAIMS_MAX_AGE_MS: u64 = 15 * 60 * 1000;

fn write_handoff_inherited_claims(version: &str, claims: &[AbandonedClaim]) {
    let path = handoff_inherited_claims_path();
    if claims.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let payload = serde_json::json!({
        "version": version,
        "pid": std::process::id(),
        "ts": now_ms(),
        "claims": claims
            .iter()
            .map(|(root, verb, task)| serde_json::json!({
                "root": root.to_string_lossy(),
                "verb": verb,
                "task": task,
            }))
            .collect::<Vec<_>>(),
    });
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, payload.to_string());
}

fn clear_handoff_inherited_claims() {
    let _ = fs::remove_file(handoff_inherited_claims_path());
}

fn read_handoff_inherited_claims() -> HashSet<AbandonedClaim> {
    let Ok(raw) = fs::read_to_string(handoff_inherited_claims_path()) else { return HashSet::new() };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&raw) else { return HashSet::new() };
    let describes_a_handoff_still_in_progress = marker
        .get("ts")
        .and_then(|t| t.as_u64())
        .map(|ts| now_ms().saturating_sub(ts) <= HANDOFF_INHERITED_CLAIMS_MAX_AGE_MS)
        .unwrap_or(false);
    if !describes_a_handoff_still_in_progress {
        return HashSet::new();
    }
    marker
        .get("claims")
        .and_then(|c| c.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some((
                        PathBuf::from(row.get("root")?.as_str()?),
                        row.get("verb")?.as_str()?.to_string(),
                        row.get("task")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn sweep_orphaned_claims(root: &Path) {
    sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &read_handoff_inherited_claims());
}

pub fn sweep_orphaned_claims_across_roots(roots: &[PathBuf]) {
    let inherited = read_handoff_inherited_claims();
    for root in roots {
        sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &inherited);
    }
    clear_handoff_inherited_claims();
}

const MIN_ORPHAN_CLAIM_AGE_MS: u64 = 600_000;

fn claim_age_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().ok()?.as_millis() as u64)
}

fn sweep_orphaned_claims_distinguishing_handoff_from_crash(root: &Path, inherited: &HashSet<AbandonedClaim>) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    if let Some(sweeper_pid) = live_foreign_spool_sweeper(&spool_dir) {
        if inherited.is_empty() {
            eprintln!(
                "[agentplug daemon] skipping orphan sweep for {} -- pid {sweeper_pid} holds a live heartbeat on this spool, so its in-flight claims are not orphans this process can see",
                root.display()
            );
            return;
        }
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
            if inherited.contains(&(root.to_path_buf(), verb.clone(), task.clone())) {
                if requeue_claim(&in_dir, &verb, &task) {
                    eprintln!("[agentplug daemon] re-queued claim {verb}/{task} for {} -- a version handoff to a confirmed-ready successor abandoned it, so it is inherited work, not orphaned work; the caller waits longer and never sees dispatch_orphaned", root.display());
                    continue;
                }
                eprintln!("[agentplug daemon] could not re-queue handoff-inherited claim {verb}/{task} for {} -- falling through to dispatch_orphaned rather than swallowing it", root.display());
            }
            if claim_age_ms(&path).map(|age| age < MIN_ORPHAN_CLAIM_AGE_MS).unwrap_or(false) {
                continue;
            }
            let out_name = format!("{verb}-{task}.json");
            if !out_dir.join(&out_name).exists() {
                let out_body = serde_json::json!({
                    "ok": false,
                    "error_code": "dispatch_orphaned",
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that stopped answering -- a wasm trap, an out-of-memory abort, or a shared-Store recycle during the call. A version handoff is NOT a cause of this error: a handoff re-queues its claims for the incoming daemon, which completes them. The outcome is UNVERIFIED, not known to be unperformed: a side-effecting verb (git_commit/git_finalize/git_push/fs_write/memorize-fire) may already have applied some or all of its work, so read the real state (git log, git status, the file, the store) before re-dispatching. Re-dispatch straight away only for a read-only verb."),
                    "verb": verb,
                    "task": task,
                    "sweeping_pid": std::process::id(),
                }).to_string();
                write_spool_out(&out_dir, &out_name, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
            }
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn sweep_unconsumable_spool_files(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let quarantine_dir = spool_dir.join("in-quarantine");
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else { return };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !file_entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if ext == Some("txt") || ext == Some(ORPHAN_CLAIM_EXT) {
                continue;
            }
            let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if file_name.is_empty() {
                continue;
            }
            let _ = fs::create_dir_all(&quarantine_dir);
            let dest = quarantine_dir.join(format!("{verb}__{file_name}"));
            if fs::rename(&path, &dest).is_ok() {
                eprintln!(
                    "[agentplug daemon] quarantined unconsumable spool file in/{verb}/{file_name} to {} -- the spool ABI is in/<verb>/<numeric-id>.txt, so a non-conforming name is never claimed by the dispatch loop and would otherwise sit invisibly forever",
                    dest.display()
                );
            }
        }
    }
}

const RAW_PLUGIN_SPOOL_VERBS: &[&str] = &["libsql", "bert"];

fn extract_session_id(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("session_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn session_id_task_mismatch_rejection(verb: &str, task: &str, body: &str) -> Option<String> {
    let declared_session_id = extract_session_id(body)?;
    let expected_prefix = format!("{declared_session_id}-");
    if task.starts_with(&expected_prefix) {
        return None;
    }
    Some(serde_json::json!({
        "ok": false,
        "error": "session_id_task_mismatch",
        "reason": format!(
            "dispatch body declared session_id {declared_session_id:?} but task id {task:?} does not start with {expected_prefix:?} -- the spool ABI requires task ids of the form <session_id>-<local-counter> so the daemon can partition claims per session; re-dispatch with a correctly prefixed task id"
        ),
        "verb": verb,
    }).to_string())
}

static LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn last_measured_dispatch_queue_wait_ms() -> u64 {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn run_gm_dispatch_to_file(root: &Path, handle: &DispatchHandle, verb: &str, task: &str, body: &str, out_dir: &Path, queue_wait_ms: u64) {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.store(queue_wait_ms, std::sync::atomic::Ordering::Relaxed);
    let plugin_name = if RAW_PLUGIN_SPOOL_VERBS.contains(&verb) { verb } else { "gm" };
    let inner_verb_owned: String = if plugin_name == "gm" {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("verb").and_then(|s| s.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "capabilities".to_string())
    };
    let _fairness_guard = GmFairnessGuard::acquire(root);
    let tool_verb = if plugin_name == "gm" { verb } else { inner_verb_owned.as_str() };
    let _tool_guard = ToolDispatchGuard::acquire(plugin_name, tool_verb);
    let dispatch_result = if plugin_name == "gm" {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch("gm", verb, body)))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch(plugin_name, &inner_verb_owned, body)))
    };
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
    let out_body = patch_update_available_from_escalation(plugin_name, verb, out_body);
    let out_name = format!("{verb}-{task}.json");
    write_spool_out(out_dir, &out_name, &out_body);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
    let key: InFlightKey = (root.to_path_buf(), verb.to_string(), task.to_string());
    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
}

fn dir_has_any_verb_subdir_with_claimable_txt(base: &Path) -> bool {
    let Ok(verb_dirs) = fs::read_dir(base) else { return false };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            if file_entry.path().extension().and_then(|e| e.to_str()) == Some("txt") {
                return true;
            }
        }
    }
    false
}

fn project_has_pending_dispatch_work(root: &Path) -> bool {
    if project_has_in_flight_step(root) {
        return true;
    }
    let pd_in = root.join(".agentplug").join("plugin-dispatch").join("in");
    if let Ok(plugin_dirs) = fs::read_dir(&pd_in) {
        for plugin_entry in plugin_dirs.flatten() {
            if !plugin_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if dir_has_any_verb_subdir_with_claimable_txt(&plugin_entry.path()) {
                return true;
            }
        }
    }
    let gm_in = root.join(".gm").join("exec-spool").join("in");
    dir_has_any_verb_subdir_with_claimable_txt(&gm_in)
}

fn dispatch_project(root: &Path, project: &mut ProjectPlugins, plugin_modules: &PluginModules) -> bool {
    let mut did_work = false;

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");

    if project_has_in_flight_step(root) {
        return false;
    }

    struct ClaimedRequest {
        verb: String,
        task: String,
        body: String,
        claimed_at: Instant,
    }
    let mut claimed: Vec<ClaimedRequest> = Vec::new();
    let in_dir_scan = fs::read_dir(&in_dir);
    let in_dir_existed = in_dir_scan.is_ok();
    if let Ok(entries) = in_dir_scan {
        'claim_one: for verb_entry in entries.flatten() {
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
                if !spool_in_file_write_has_settled(&file_path) {
                    continue;
                }
                let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                let claimed_at = Instant::now();
                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                if body.trim().is_empty() {
                    let _ = fs::rename(&claim_path, &file_path);
                    continue;
                }
                did_work = true;
                claimed.push(ClaimedRequest { verb: verb.clone(), task, body, claimed_at });
                break 'claim_one;
            }
        }
    }

    if claimed.is_empty() && in_dir_existed && !project_has_pending_dispatch_work(root) {
        return did_work;
    }

    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }
    write_project_heartbeat(&spool_dir, read_status_busy_until_if_future(&spool_dir));

    let requested_plugins = {
        let mut list = vec![
            "gm".to_string(),
            "libsql".to_string(),
            "bert".to_string(),
            "treesitter".to_string(),
            "oxibrowser".to_string(),
            "crux".to_string(),
        ];
        for extra in read_project_plugin_list(root) {
            if !list.contains(&extra) {
                list.push(extra);
            }
        }
        list
    };

    let mut gm_requests: Vec<ClaimedRequest> = Vec::with_capacity(claimed.len());
    let mut bg_convert_requests: Vec<ClaimedRequest> = Vec::new();
    let mut plugin_refresh_requests: Vec<ClaimedRequest> = Vec::new();
    for req in claimed {
        if let Some(out_body) = session_id_task_mismatch_rejection(&req.verb, &req.task, &req.body) {
            let out_name = format!("{}-{}.json", req.verb, req.task);
            write_spool_out(&out_dir, &out_name, &out_body);
            let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
            continue;
        }
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
        let mut gm_load_failure_reason: Option<String> = None;
        for plugin_name in &requested_plugins {
            if project.is_loaded(plugin_name) {
                continue;
            }
            let Some((module, content_hash)) = plugin_modules.module_with_hash(plugin_name) else {
                let reason = match read_plugin_compile_failure(plugin_name) {
                    Some(compile_err) => format!("plugin {plugin_name} failed to compile/install: {compile_err}"),
                    None => format!("plugin {plugin_name} not yet compiled for {}: dispatch this thread's own get_or_compile could not run against the shared PluginModules from a worker thread -- see plugin_modules.get_or_compile() call in run_daemon's pre-chunk warm pass", root.display()),
                };
                eprintln!("[agentplug daemon] {reason}");
                if plugin_name == "gm" { gm_load_failure_reason = Some(reason); }
                continue;
            };
            if let Err(e) = project.load_plugin(&plugin_modules.engine, plugin_name, module, content_hash) {
                let reason = format!("failed to instantiate plugin {plugin_name} for {}: {e:#}", root.display());
                eprintln!("[agentplug daemon] {reason}");
                match crate::download::record_plugin_load_failure_and_rollback(plugin_name) {
                    Ok(true) => {
                        eprintln!(
                            "[agentplug daemon] {plugin_name} rolled back after instantiate failure -- retry this dispatch; the rolled-back version will compile and load on the next attempt"
                        );
                    }
                    Ok(false) => {
                        eprintln!(
                            "[agentplug daemon] {plugin_name} instantiate failure has no prior working version to roll back to (first install, or no .wasm.prev backup exists) -- cannot self-recover"
                        );
                    }
                    Err(rollback_err) => {
                        eprintln!(
                            "[agentplug daemon] {plugin_name} rollback after instantiate failure itself failed: {rollback_err:#}"
                        );
                    }
                }
                if plugin_name == "gm" { gm_load_failure_reason = Some(reason); }
            }
        }

        if !project.is_loaded("gm") {
            let error_message = match &gm_load_failure_reason {
                Some(reason) => format!("gm plugin failed to load for this project: {reason}"),
                None => "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)".to_string(),
            };
            for req in &gm_requests {
                let out_name = format!("{}-{}.json", req.verb, req.task);
                let out_body = serde_json::json!({"ok": false, "error": error_message, "verb": req.verb}).to_string();
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
                let queue_wait_ms = req.claimed_at.elapsed().as_millis() as u64;
                let join_handle = std::thread::spawn(move || {
                    run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir, queue_wait_ms);
                });
                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
            }

            answer_bg_converts(bg_convert_requests);

            const WORKER_AUTO_DETACH_AFTER_MS: u64 = 45_000;
            const STATUS_REFRESH_INTERVAL_MS: u64 = 5_000;
            const PROJECT_BATCH_ABSORB_WINDOW_MS: u64 = 0;
            let batch_deadline = Instant::now() + Duration::from_millis(PROJECT_BATCH_ABSORB_WINDOW_MS);
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
                        write_project_heartbeat(&spool_dir, Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS));
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
                    if Instant::now() < batch_deadline {
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
                                    let claimed_at = Instant::now();
                                    let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                    let body = fs::read_to_string(&claim_path).unwrap_or_default();

                                    if let Some(out_body) = session_id_task_mismatch_rejection(&verb, &task, &body) {
                                        let out_name = format!("{verb}-{task}.json");
                                        write_spool_out(&out_dir, &out_name, &out_body);
                                        let _ = fs::remove_file(&claim_path);
                                        continue;
                                    }

                                    let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((plugin_modules.engine.clone(), plugin_modules.modules_with_hashes())));
                                    let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    let key: InFlightKey = (root.to_path_buf(), verb.clone(), task.clone());
                                    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                                    let thread_root = root.to_path_buf();
                                    let thread_verb = verb.clone();
                                    let thread_task = task.clone();
                                    let thread_body = body;
                                    let thread_out_dir = out_dir.clone();
                                    let queue_wait_ms = claimed_at.elapsed().as_millis() as u64;
                                    let join_handle = std::thread::spawn(move || {
                                        run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir, queue_wait_ms);
                                    });
                                    spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
                                }
                            }
                        }
                    }

                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    if did_work {
        return true;
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

                {
                    let current = plugin_modules.module_with_hash(&plugin_name)
                        .map(|(_, hash)| project.is_loaded_current(&plugin_name, hash))
                        .unwrap_or_else(|| project.is_loaded(&plugin_name));
                    if !current {
                        let Some((module, content_hash)) = plugin_modules.module_with_hash(&plugin_name) else {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        };
                        if let Err(e) = project.load_plugin(&plugin_modules.engine, &plugin_name, module, content_hash) {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        }
                    }
                }

                if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&DaemonConfig::load()) {
                    let mut released: Vec<&str> = Vec::new();
                    for shared_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
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

                let _tool_guard = ToolDispatchGuard::acquire(&plugin_name, &verb);
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
                let out_body = patch_update_available_from_escalation(&plugin_name, &verb, out_body);
                write_pd_out(&out_name, &out_body);
                return true;
            }
        }
    }

    did_work
}

pub fn try_dispatch_via_daemon(cwd: &Path, plugin: &str, verb: &str, body: &str) -> Option<String> {
    if std::env::var("AGENTPLUG_NO_DAEMON").is_ok() {
        return None;
    }
    if let Err(e) = register_project(cwd) {
        eprintln!("[agentplug] {e}");
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

fn seed_github_token_from_gh_cli_if_unset() {
    if std::env::var_os("GITHUB_TOKEN").is_some() || std::env::var_os("GH_TOKEN").is_some() {
        return;
    }
    let mut gh_cmd = std::process::Command::new("gh");
    gh_cmd.args(["auth", "token"]);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        gh_cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(output) = gh_cmd.output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return;
    }
    std::env::set_var("GH_TOKEN", &token);
    eprintln!("[agentplug daemon] seeded GH_TOKEN from `gh auth token` -- ci-status and other GitHub API verbs now run authenticated, avoiding the unauthenticated 60/hr rate limit");
}

pub fn run_daemon() -> anyhow::Result<()> {
    if let Some(owner_pid) = shared_daemon_owner_that_would_refuse_this_process() {
        record_wasted_daemon_start();
        eprintln!(
            "[agentplug daemon] shared daemon pid {owner_pid} already owns the ownership lock and its heartbeat is fresh -- exiting before the registry announce and the `gh auth token` seed, nothing shared was touched"
        );
        return Ok(());
    }

    eprintln!("[agentplug daemon] starting, registry {}", registry_path().display());

    if !claim_ownership() {
        record_wasted_daemon_start();
        let existing_pid = read_owner_pid();
        eprintln!(
            "[agentplug daemon] lost the atomic ownership claim -- pid {:?} already owns the shared daemon, exiting before touching any shared plugin state",
            existing_pid
        );
        return Ok(());
    }

    clear_wasted_daemon_start_backoff();
    seed_github_token_from_gh_cli_if_unset();

    let plugin_modules = PluginModules::new()?;
    let previously_recorded_version = installed_runner_version();
    if previously_recorded_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        crate::download::clear_all_known_bad_version_markers();
        let _ = record_runner_version(env!("CARGO_PKG_VERSION"));
    }
    run_daemon_body(plugin_modules)
}

fn run_daemon_body(mut plugin_modules: PluginModules) -> anyhow::Result<()> {
    HEARTBEAT_DAEMON_BOOT_TS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    write_daemon_heartbeat(0, 0);
    eprintln!(
        "[agentplug daemon] BOOT pid={} version={} ts={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        now_ms()
    );

    let daemon_cfg = DaemonConfig::load();
    let registry_poll_interval = daemon_cfg.registry_poll_interval();
    let heartbeat_interval = daemon_cfg.heartbeat_interval();
    GM_PROCESSOR_CAPACITY.store(daemon_cfg.gm_pool_size(), std::sync::atomic::Ordering::Relaxed);
    SHARED_STORE_RECYCLE_LIMIT_MB.store(daemon_cfg.shared_store_recycle_private_bytes() / (1024 * 1024), std::sync::atomic::Ordering::Relaxed);
    *gm_processor_capacity_reason().lock().unwrap_or_else(|e| e.into_inner()) = daemon_cfg.gm_pool_capacity_reason();
    eprintln!(
        "[agentplug daemon] concurrency: max_concurrent_projects={} gm_concurrency={} gm_pool_size={} side_plugin_concurrency={} (host_available_parallelism={}, unset config keys derive from it)",
        daemon_cfg.max_concurrent_projects(),
        daemon_cfg.gm_concurrency(),
        daemon_cfg.gm_pool_size(),
        daemon_cfg.side_plugin_concurrency(),
        host_available_parallelism()
    );
    agentplug_host::set_gm_pool_size(daemon_cfg.gm_pool_size());
    agentplug_host::set_side_plugin_pool_size(daemon_cfg.side_plugin_concurrency());

    crate::download::gc_stale_tmp_files(Duration::from_secs(60 * 60));

    const COLD_PROJECT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now();
    let mut first_registry_poll_pending = true;
    let mut known_roots: Vec<PathBuf> = Vec::new();
    let mut roots_new_this_registry_poll: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut last_cold_project_sweep = Instant::now().checked_sub(COLD_PROJECT_SWEEP_INTERVAL).unwrap_or_else(Instant::now);
    let mut project_round_robin_cursor = 0usize;

    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    let shared_plugin_release_idle_ms = daemon_cfg.shared_plugin_release_idle_ms();
    let mut last_shared_release = Instant::now();

    let plugin_update_poll_interval = daemon_cfg.plugin_update_poll_interval();
    let instruction_source_poll_interval = daemon_cfg.instruction_source_poll_interval();
    let shortest_plugin_poll_interval = daemon_cfg
        .plugin_update_poll_interval_secs_by_name
        .values()
        .copied()
        .min()
        .map(Duration::from_secs)
        .map(|per_plugin| per_plugin.min(plugin_update_poll_interval))
        .unwrap_or(plugin_update_poll_interval);
    let mut last_plugin_specific_poll: HashMap<String, Instant> = HashMap::new();
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
    let mut pending_self_update_staged_at: Option<Instant> = None;
    const SELF_UPDATE_MAX_STARVED_MS: u64 = 10 * 60 * 1000;

    if let Some((staged_at_ms, _len)) = staged_runner_awaiting_handoff() {
        if let Some(staged_path) = canonical_runner_exe_path().map(|c| {
            c.with_extension(c.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string()))
        }) {
            let staged_age = now_ms().saturating_sub(staged_at_ms);
            let mut boot_check_cmd = std::process::Command::new(&staged_path);
            boot_check_cmd.arg("--version");
            #[cfg(windows)]
            {
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                boot_check_cmd.creation_flags(CREATE_NO_WINDOW);
            }
            match boot_check_cmd.output() {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout).trim().trim_start_matches('v').to_string();
                    eprintln!(
                        "[agentplug daemon] found pre-existing staged runner {} (version {version}) at boot, age {}ms -- adopting its on-disk mtime so a daemon restart does not reset the starve clock",
                        staged_path.display(), staged_age
                    );
                    pending_self_update = Some((staged_path, version));
                    pending_self_update_staged_at = Instant::now().checked_sub(Duration::from_millis(staged_age));
                }
                Ok(out) => {
                    eprintln!(
                        "[agentplug daemon] pre-existing staged runner {} at boot failed --version check (exit {}) -- removing stale/corrupt staged binary",
                        staged_path.display(), out.status
                    );
                    let _ = fs::remove_file(&staged_path);
                }
                Err(e) => {
                    eprintln!(
                        "[agentplug daemon] pre-existing staged runner {} at boot could not be spawned for --version ({e}) -- removing stale/corrupt staged binary",
                        staged_path.display()
                    );
                    let _ = fs::remove_file(&staged_path);
                }
            }
        }
    }

    let mut last_instruction_source_sync: HashMap<PathBuf, Instant> = HashMap::new();
    let mut instruction_source_syncing: HashSet<PathBuf> = HashSet::new();
    let (instruction_source_sync_done_tx, instruction_source_sync_done_rx) = std::sync::mpsc::channel::<PathBuf>();

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
            let requeued = hand_claims_to_live_successor("heartbeat-authority-holder");
            sweep_orphaned_claims_across_roots(&known_roots);
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- re-queued {requeued} in-flight claim(s) for it and exiting before serving further work");
            return Ok(());
        }

        if first_registry_poll_pending || last_registry_poll.elapsed() >= registry_poll_interval {
            let sweep_orphans_left_by_whatever_daemon_died_before_answering = first_registry_poll_pending;
            first_registry_poll_pending = false;
            last_registry_poll = Instant::now();
            let previous_roots: std::collections::HashSet<PathBuf> = known_roots.iter().cloned().collect();
            known_roots = read_registry();
            roots_new_this_registry_poll = known_roots.iter().filter(|r| !previous_roots.contains(*r)).cloned().collect();
            set_known_project_roots(&known_roots);
            if sweep_orphans_left_by_whatever_daemon_died_before_answering {
                sweep_orphaned_claims_across_roots(&known_roots);
                for root in &known_roots {
                    sweep_unconsumable_spool_files(root);
                }
            }
        }

        while let Ok(root) = instruction_source_sync_done_rx.try_recv() {
            instruction_source_syncing.remove(&root);
        }

        const BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS: u64 = 5 * 60 * 1000;
        if last_browser_orphan_sweep.elapsed() >= Duration::from_millis(BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS) {
            last_browser_orphan_sweep = Instant::now();
            agentplug_host::reap_idle_sessions_and_os_orphans_across_every_known_project_root(&known_roots);
        }

        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

        for root in &known_roots {
            for plugin_name in read_project_plugin_list(root) {
                if plugin_compile_in_backoff(&plugin_name) {
                    continue;
                }
                match plugin_modules.get_or_compile(&plugin_name) {
                    Ok(()) => clear_plugin_compile_failure(&plugin_name),
                    Err(e) => {
                        eprintln!("[agentplug daemon] failed to compile/install plugin {plugin_name} for {}: {e:#}", root.display());
                        record_plugin_compile_failure(&plugin_name, format!("{e:#}"));
                    }
                }
            }
            let due = last_instruction_source_sync
                .get(root)
                .map(|t| t.elapsed() >= instruction_source_poll_interval)
                .unwrap_or(true);
            if due && instruction_source_syncing.insert(root.clone()) {
                last_instruction_source_sync.insert(root.clone(), Instant::now());
                let thread_root = root.clone();
                let done = instruction_source_sync_done_tx.clone();
                std::thread::spawn(move || {
                    if let Err(e) = sync_instruction_source_if_configured(&thread_root) {
                        eprintln!("[agentplug daemon] instruction source-repo sync failed for {}: {e:#}", thread_root.display());
                    }
                    let _ = done.send(thread_root);
                });
            }
        }
        for plugin_name in ["gm", "libsql", "bert", "treesitter", "oxibrowser", "crux"] {
            if plugin_compile_in_backoff(plugin_name) {
                continue;
            }
            match plugin_modules.get_or_compile(plugin_name) {
                Ok(()) => clear_plugin_compile_failure(plugin_name),
                Err(e) => {
                    eprintln!("[agentplug daemon] failed to compile/install default plugin {plugin_name}: {e:#}");
                    record_plugin_compile_failure(plugin_name, format!("{e:#}"));
                }
            }
        }

        let sweep_cold_this_tick = last_cold_project_sweep.elapsed() >= COLD_PROJECT_SWEEP_INTERVAL;
        if sweep_cold_this_tick {
            last_cold_project_sweep = Instant::now();
        }
        let mut all_projects: Vec<(PathBuf, ProjectPlugins)> = Vec::with_capacity(known_roots.len());
        let mut is_genuinely_active: Vec<bool> = Vec::with_capacity(known_roots.len());
        let mut skipped_cold = 0usize;
        for root in &known_roots {
            match projects.remove(root) {
                Some(p) => {
                    all_projects.push((root.clone(), p));
                    is_genuinely_active.push(project_has_pending_dispatch_work(root));
                }
                None if sweep_cold_this_tick
                    || roots_new_this_registry_poll.contains(root)
                    || project_has_pending_dispatch_work(root) =>
                {
                    all_projects.push((root.clone(), ProjectPlugins::new(root.clone())));
                    is_genuinely_active.push(
                        roots_new_this_registry_poll.contains(root) || project_has_pending_dispatch_work(root),
                    );
                }
                None => skipped_cold += 1,
            }
        }
        let worker_count = max_concurrent_projects.min(all_projects.len().max(1));
        let queue_total = all_projects.len();
        let active_roots: Vec<(usize, &PathBuf)> = is_genuinely_active
            .iter()
            .enumerate()
            .filter(|(_, active)| **active)
            .map(|(i, _)| (i, &all_projects[i].0))
            .collect();
        let reported_queue_total = active_roots.len();
        if skipped_cold > 0 {
            eprintln!("[agentplug daemon] cold-project sweep: {skipped_cold} project(s) with no recent activity skipped this tick, {queue_total} project(s) rescanned ({reported_queue_total} genuinely active)");
        }
        if reported_queue_total > worker_count {
            for (position, (_, root)) in active_roots.iter().enumerate() {
                let spool_dir = root.join(".gm").join("exec-spool");
                if fs::create_dir_all(&spool_dir).is_ok() {
                    write_project_heartbeat_with_queue_info(&spool_dir, read_status_busy_until_if_future(&spool_dir), Some((position, reported_queue_total)));
                }
            }
        }
        let mut background_projects = Vec::with_capacity(all_projects.len());
        let mut active_projects = Vec::with_capacity(all_projects.len());
        for (project, active) in all_projects.into_iter().zip(is_genuinely_active) {
            if active {
                active_projects.push(project);
            } else {
                background_projects.push(project);
            }
        }
        if !active_projects.is_empty() {
            let len = active_projects.len();
            active_projects.rotate_left(project_round_robin_cursor % len);
            project_round_robin_cursor = (project_round_robin_cursor + worker_count) % len;
        }
        background_projects.extend(active_projects);
        let queue = std::sync::Mutex::new(background_projects);
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
            if reported_queue_total > worker_count {
                let spool_dir = root.join(".gm").join("exec-spool");
                if fs::create_dir_all(&spool_dir).is_ok() {
                    write_project_heartbeat_with_queue_info(&spool_dir, read_status_busy_until_if_future(&spool_dir), Some((0, 0)));
                }
            }
            projects.insert(root, project);
        }
        HEARTBEAT_PROJECT_COUNT.store(projects.len(), std::sync::atomic::Ordering::Relaxed);
        HEARTBEAT_PLUGIN_MODULE_COUNT.store(plugin_modules.modules.len(), std::sync::atomic::Ordering::Relaxed);
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            return Ok(());
        }
        let evict_before = Instant::now().checked_sub(Duration::from_millis(daemon_cfg.project_idle_evict_ms())).unwrap_or_else(Instant::now);
        let to_evict: Vec<PathBuf> = projects.iter().filter(|(_, p)| p.last_active < evict_before).map(|(root, _)| root.clone()).collect();
        for root in to_evict {
            eprintln!("[agentplug daemon] evicting idle project {}", root.display());
            projects.remove(&root);
        }

        let forced_refresh_request = take_forced_plugin_refresh_request();
        if last_plugin_update_poll.elapsed() >= shortest_plugin_poll_interval || forced_refresh_request.is_some() {
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
                let forced = matches!(&forced_refresh_request, Some(Some(name)) if name == &plugin_name);
                if !forced {
                    let due = last_plugin_specific_poll
                        .get(&plugin_name)
                        .map(|t| t.elapsed() >= daemon_cfg.plugin_update_poll_interval_for(&plugin_name))
                        .unwrap_or(true);
                    if !due {
                        continue;
                    }
                }
                last_plugin_specific_poll.insert(plugin_name.clone(), Instant::now());
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
                    if pending_self_update.is_none() {
                        pending_self_update_staged_at = Some(Instant::now());
                    }
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

        let self_update_starved = pending_self_update_staged_at
            .map(|staged_at| staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_MAX_STARVED_MS))
            .unwrap_or(false);
        let detached_still_running = !in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).is_empty();
        const SELF_UPDATE_HARD_CAP_MS: u64 = SELF_UPDATE_MAX_STARVED_MS + 60_000;
        let self_update_hard_capped = pending_self_update_staged_at
            .map(|staged_at| staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_HARD_CAP_MS))
            .unwrap_or(false);
        let force_handoff_despite_in_flight = self_update_starved && detached_still_running && self_update_hard_capped;
        let force_handoff_never_idle = self_update_starved && self_update_hard_capped && !detached_still_running && any_work;
        if (!any_work && !detached_still_running) || force_handoff_despite_in_flight || force_handoff_never_idle {
            if let Some((staged, version)) = pending_self_update.take() {
                let claims_the_successor_inherits = snapshot_in_flight_claims();
                write_handoff_inherited_claims(&version, &claims_the_successor_inherits);
                if force_handoff_despite_in_flight {
                    eprintln!(
                        "[agentplug daemon] self-update to {version} starved for {}ms with in-flight dispatches still running after the extra grace window -- forcing handoff and re-queueing {} in-flight dispatch(es) for the incoming version; their callers wait longer and never see dispatch_orphaned",
                        SELF_UPDATE_HARD_CAP_MS,
                        claims_the_successor_inherits.len()
                    );
                }
                if force_handoff_never_idle {
                    eprintln!(
                        "[agentplug daemon] self-update to {version} starved for {}ms with the daemon continuously busy (any_work true every tick, nothing detached) -- forcing handoff at the next tick boundary rather than waiting indefinitely for an idle tick that a busy shared daemon may never reach",
                        SELF_UPDATE_HARD_CAP_MS
                    );
                }
                if attempt_self_update_handoff(&staged, &version) {
                    agentplug_host::close_all_sessions();
                    let requeued = requeue_claims_for_live_successor(&claims_the_successor_inherits);
                    eprintln!(
                        "[agentplug daemon] handed off to version {version} -- re-queued {requeued} of {} inherited claim(s) for the incoming daemon, exiting",
                        claims_the_successor_inherits.len()
                    );
                    return Ok(());
                }
                clear_handoff_inherited_claims();
                pending_self_update = Some((staged, version));
            }
        }

        if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&daemon_cfg) {
            let mut released: Vec<&str> = Vec::new();
            for plugin_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
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
        } else if last_shared_release.elapsed() >= Duration::from_millis(shared_plugin_release_idle_ms) {
            let mut released: Vec<&str> = Vec::new();
            for plugin_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
                if agentplug_host::release_shared_plugin(plugin_name) {
                    released.push(plugin_name);
                }
            }
            if !released.is_empty() {
                eprintln!(
                    "[agentplug daemon] released idle shared Stores [{}] after {}ms quiet -- returns their grown wasm linear memory; next call re-instantiates",
                    released.join(", "),
                    shared_plugin_release_idle_ms
                );
            }
            last_shared_release = Instant::now();
        }

        if any_work {
            last_any_dispatch = Instant::now();
        } else if last_any_dispatch.elapsed() >= Duration::from_millis(SELF_RECYCLE_IDLE_MS) && !detached_still_running {
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
