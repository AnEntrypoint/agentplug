use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use wasmtime::{Engine, Module, Trap};

use agentplug_host::{
    build_engine, dispatch_serial_lane, install_dir, now_ms, read_project_plugin_list,
    DispatchHandle, GmFairnessGuard, LaneWaitReport, ProjectPlugins, ToolDispatchGuard,
    ToolQueueWaitReport,
};

use crate::download::{
    ensure_plugin_installed, installed_plugin_version, installed_runner_version,
    is_recognized_release_semver, record_runner_version,
};

fn registry_path() -> PathBuf {
    install_dir().join("daemon-registry.txt")
}

const GM_SPOOL_VERBS: &[&str] = &[
    "instruction",
    "transition",
    "phase-status",
    "prd-add",
    "prd-list",
    "prd-resolve",
    "prd-status",
    "mutable-add",
    "mutable-list",
    "mutable-resolve",
    "fs_read",
    "fs_write",
    "fs_readdir",
    "fs_stat",
    "scan_deps",
    "fetch",
    "env_get",
    "kv_get",
    "kv_put",
    "kv_query",
    "exec_js",
    "lang",
    "health",
    "config_resolve",
    "config-sync-now",
    "dataflow_resolve",
    "sql_open",
    "sql_close",
    "sql_list_dbs",
    "sql_exec",
    "sql_query",
    "sql_smoke",
    "sql_serialize",
    "sql_deserialize",
    "cache_get",
    "cache_put",
    "cache_invalidate",
    "cache_stats",
    "codeinsight_index",
    "codesearch",
    "callers",
    "callees",
    "impact",
    "memorize",
    "memorize-prune",
    "memorize-vacuum",
    "memorize-retention",
    "recall",
    "tencentdb-compat-probe",
    "tencentdb-memory-import",
    "python",
    "bash",
    "powershell",
    "ssh",
    "go",
    "rust",
    "c",
    "cpp",
    "java",
    "deno",
    "status",
    "wait",
    "close",
    "filter",
    "git_status",
    "branch_status",
    "git_push",
    "git_add",
    "git_commit",
    "git_finalize",
    "git_log",
    "git_diff",
    "git_show",
    "git_fetch",
    "git_pull",
    "ci-status",
    "git_branch",
    "git_checkout",
    "git_merge",
    "git_merge_abort",
    "git_branch_delete",
    "git_rm",
    "git_revert",
    "git_reset",
    "git_reset_head",
    "git_poll",
    "git_worktree_add",
    "git_worktree_list",
    "git_worktree_remove",
    "git_worktree_prune",
    "forget",
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
        && cwd
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/.gm/exec-spool")
}

pub fn register_project(cwd: &Path) -> anyhow::Result<()> {
    let cwd = agentplug_host::project_root(cwd);
    if cwd_is_inside_a_spool_tree(&cwd) {
        anyhow::bail!(
            "refusing to register {} as a project root -- its own path is already inside a .gm/exec-spool tree, which means this is spool runtime state (in/out/status files), not a genuine project directory. Launch the spool from the actual project root instead.",
            cwd.display()
        );
    }
    provision_gm_spool_verb_dirs(&cwd)?;
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let cwd_str = agentplug_host::canonical_project_root(&cwd)
        .to_string_lossy()
        .to_string();

    let mut live: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut respelled = false;
    for line in existing.lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        if !Path::new(entry).exists() {
            dropped += 1;
            continue;
        }
        let canonical = cached_project_root(entry).to_string_lossy().to_string();
        respelled |= canonical != entry;
        if live.iter().any(|e| e == &canonical) {
            respelled = true;
            continue;
        }
        live.push(canonical);
    }

    let already_present = live.iter().any(|e| e == &cwd_str);
    if already_present && dropped == 0 && !respelled {
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

fn describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(
    e: &anyhow::Error,
) -> String {
    match e.downcast_ref::<Trap>() {
        Some(trap) => format!("[wasm trap: {trap}] {e:#}"),
        None => format!("{e:#}"),
    }
}

const REGISTRY_ENTRY_ROOT_CACHE_TTL: Duration = Duration::from_secs(600);

fn cached_project_root(entry: &str) -> PathBuf {
    static SLOT: OnceLock<Mutex<HashMap<String, (Instant, PathBuf)>>> = OnceLock::new();
    let cache = SLOT.get_or_init(|| Mutex::new(HashMap::new()));
    let fresh = cache
        .lock()
        .ok()
        .and_then(|entries| entries.get(entry).cloned())
        .filter(|(resolved_at, _)| resolved_at.elapsed() < REGISTRY_ENTRY_ROOT_CACHE_TTL)
        .map(|(_, root)| root);
    if let Some(root) = fresh {
        return root;
    }
    let root = agentplug_host::project_root(Path::new(entry));
    if let Ok(mut entries) = cache.lock() {
        entries.insert(entry.to_string(), (Instant::now(), root.clone()));
    }
    root
}

pub(crate) fn read_registry() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for entry in fs::read_to_string(registry_path())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
    {
        if entry.is_empty() || !Path::new(entry).exists() {
            continue;
        }
        let canonical = cached_project_root(entry);
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    roots
}

fn host_available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
}

const DEFAULT_GM_POOL_SIZE: usize = 8;
const MAX_GM_POOL_SIZE: usize = 16;

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
  "runner_update_poll_interval_secs": 60,
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
    fn registry_poll_interval(&self) -> Duration {
        Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5))
    }
    fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10))
    }
    fn plugin_update_poll_interval(&self) -> Duration {
        Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600))
    }
    fn plugin_update_poll_interval_for(&self, plugin_name: &str) -> Duration {
        match self
            .plugin_update_poll_interval_secs_by_name
            .get(plugin_name)
        {
            Some(secs) => Duration::from_secs(*secs),
            None => self.plugin_update_poll_interval(),
        }
    }
    fn runner_update_poll_interval(&self) -> Duration {
        Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(60))
    }
    fn instruction_source_poll_interval(&self) -> Duration {
        Duration::from_secs(self.instruction_source_poll_interval_secs.unwrap_or(600))
    }
    fn max_concurrent_projects(&self) -> usize {
        4
    }
    fn gm_concurrency(&self) -> usize {
        self.gm_concurrency
            .unwrap_or_else(|| self.max_concurrent_projects())
            .max(1)
    }
    fn gm_pool_size(&self) -> usize {
        self.gm_pool_size
            .unwrap_or(DEFAULT_GM_POOL_SIZE)
            .min(host_available_parallelism())
            .min(MAX_GM_POOL_SIZE)
            .max(1)
    }

    fn gm_pool_capacity_reason(&self) -> String {
        let capacity = self.gm_pool_size();
        format!("{capacity} processor(s): default {DEFAULT_GM_POOL_SIZE}, capped by host parallelism and {MAX_GM_POOL_SIZE}, gm_pool_size in daemon-config.json overrides")
    }
    fn side_plugin_concurrency(&self) -> usize {
        self.side_plugin_concurrency.unwrap_or(1).max(1)
    }
    fn shared_store_recycle_private_bytes(&self) -> u64 {
        const DEFAULT_MB: u64 = 1600;
        self.shared_store_recycle_private_mb
            .unwrap_or(DEFAULT_MB)
            .max(256)
            * 1024
            * 1024
    }
    fn shared_store_recycle_dispatches(&self) -> u64 {
        let default = 500u64.saturating_mul(self.gm_concurrency() as u64).max(100);
        self.shared_store_recycle_dispatches
            .unwrap_or(default)
            .max(1)
    }
    fn project_idle_evict_ms(&self) -> u64 {
        const DEFAULT_SECS: u64 = 30 * 60;
        self.project_idle_evict_secs.unwrap_or(DEFAULT_SECS).max(60) * 1000
    }
    fn shared_plugin_release_idle_ms(&self) -> u64 {
        const DEFAULT_SECS: u64 = 30 * 60;
        const MIN_SECS: u64 = 5 * 60;
        self.shared_plugin_release_idle_secs
            .unwrap_or(DEFAULT_SECS)
            .max(MIN_SECS)
            * 1000
    }
}

fn shared_store_recycle_reason_independent_of_daemon_idle_state(
    cfg: &DaemonConfig,
) -> Option<String> {
    let dispatches = agentplug_host::shared_dispatches_since_release();
    if let Some(private_bytes) =
        agentplug_host::process_private_bytes_tracking_retained_wasm_peak_unlike_working_set()
    {
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
        return Some(format!(
            "dispatch budget: {dispatches} shared dispatches >= {dispatch_limit} limit"
        ));
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

fn handoff_reservation_path() -> PathBuf {
    install_dir().join("daemon-handoff-reservation.json")
}

const HANDOFF_RESERVATION_MAX_AGE_MS: u64 = 30_000;

fn handoff_reservation_blocks(pid: u64) -> bool {
    let reservation = fs::read_to_string(handoff_reservation_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let Some(reservation) = reservation else {
        return false;
    };
    let Some(reserved_pid) = reservation.get("pid").and_then(|value| value.as_u64()) else {
        return false;
    };
    let ts = reservation
        .get("ts")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    reserved_pid != pid
        && now_ms().saturating_sub(ts) < HANDOFF_RESERVATION_MAX_AGE_MS
        && pid_is_alive(reserved_pid)
}

fn reserve_ownership_for_handoff(pid: u64, version: &str) -> anyhow::Result<()> {
    if pid == 0 || !pid_is_alive(pid) {
        anyhow::bail!("successor pid {pid} is not alive");
    }
    let path = handoff_reservation_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(
        &temp,
        serde_json::json!({ "pid": pid, "version": version, "ts": now_ms() }).to_string(),
    )?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn read_owner_pid() -> Option<u64> {
    fs::read_to_string(daemon_owner_path())
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

pub fn claim_ownership() -> bool {
    let owner_path = daemon_owner_path();
    if let Some(parent) = owner_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let my_pid = std::process::id() as u64;
    if handoff_reservation_blocks(my_pid) {
        return false;
    }

    if fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&owner_path)
        .is_ok()
    {
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
    let payload =
        serde_json::json!({ "ts": now_ms(), "wasted_starts": wasted_starts.saturating_add(1) });
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

fn intentional_exit_path() -> PathBuf {
    install_dir().join("daemon-intentional-exit.json")
}

pub fn mark_intentional_exit(kind: &str) {
    let path = intentional_exit_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "ts": now_ms(), "pid": std::process::id(), "kind": kind });
    let _ = fs::write(&path, payload.to_string());
}

fn intentional_exit_marker_age_ms() -> Option<u64> {
    fs::read_to_string(intentional_exit_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("ts").and_then(|t| t.as_u64()))
        .map(|ts| now_ms().saturating_sub(ts))
}

fn guard_lock_path() -> PathBuf {
    install_dir().join("daemon-guard.lock")
}

fn live_guard_pid() -> Option<u64> {
    let pid = fs::read_to_string(guard_lock_path())
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    if pid == std::process::id() as u64 {
        return None;
    }
    if pid_is_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn claim_guard_lock() -> bool {
    if live_guard_pid().is_some() {
        return false;
    }
    let path = guard_lock_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("lock.tmp.{}", std::process::id()));
    if fs::write(&tmp, std::process::id().to_string()).is_err() {
        return false;
    }
    if fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        == Some(std::process::id() as u64)
}

pub fn ensure_daemon_guard() {
    if live_guard_pid().is_some() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[agentplug daemon] guard not armed: own exe path unavailable ({e})");
            return;
        }
    };
    if let Err(e) = spawn_detached(&exe, &["daemon-guard"]) {
        eprintln!("[agentplug daemon] guard not armed: spawning it failed ({e})");
        return;
    }
    eprintln!(
        "[agentplug daemon] guard armed: a heartbeat that stops without an intentional-exit marker is restarted automatically"
    );
}

const GUARD_POLL_MS: u64 = 2_000;
const GUARD_START_WAIT_MS: u64 = 45_000;
const GUARD_MAX_RESTARTS: u32 = 20;
const INTENTIONAL_EXIT_HONOR_MS: u64 = 10_000;

pub fn run_daemon_guard() -> anyhow::Result<()> {
    if !claim_guard_lock() {
        eprintln!("[agentplug daemon-guard] a guard is already live -- exiting");
        return Ok(());
    }
    eprintln!(
        "[agentplug daemon-guard] pid {} watching the shared daemon heartbeat",
        std::process::id()
    );
    let mut restarts = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(GUARD_POLL_MS));
        if is_daemon_fresh() {
            continue;
        }
        if let Some(age) = intentional_exit_marker_age_ms() {
            if age < INTENTIONAL_EXIT_HONOR_MS {
                eprintln!(
                    "[agentplug daemon-guard] daemon stopped by design (intentional-exit marker {age}ms old) -- standing down; the next dispatch starts a fresh daemon"
                );
                let _ = fs::remove_file(intentional_exit_path());
                return Ok(());
            }
        }
        if restarts >= GUARD_MAX_RESTARTS {
            eprintln!(
                "[agentplug daemon-guard] gave up after {restarts} restart attempt(s) -- exiting so a later daemon boot arms a fresh guard"
            );
            return Ok(());
        }
        restarts += 1;
        clear_wasted_daemon_start_backoff();
        eprintln!(
            "[agentplug daemon-guard] daemon heartbeat is stale with no intentional-exit marker -- restarting it (attempt {restarts}/{GUARD_MAX_RESTARTS})"
        );
        if let Err(e) = spawn_detached_daemon() {
            eprintln!(
                "[agentplug daemon-guard] restart attempt {restarts} could not spawn a daemon: {e}"
            );
            continue;
        }
        let mut became_fresh = false;
        for _ in 0..(GUARD_START_WAIT_MS / 500) {
            std::thread::sleep(Duration::from_millis(500));
            if is_daemon_fresh() {
                became_fresh = true;
                break;
            }
        }
        if !became_fresh {
            eprintln!(
                "[agentplug daemon-guard] restart attempt {restarts} published no heartbeat within {GUARD_START_WAIT_MS}ms -- retrying"
            );
        }
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
    let acquired = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .is_ok();
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
    let Ok(raw) = fs::read_to_string(daemon_status_path()) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(ts) = v.get("ts").and_then(|t| t.as_u64()) else {
        return false;
    };
    if now_ms().saturating_sub(ts) >= DAEMON_STALE_MS {
        return false;
    }
    let Some(pid) = v.get("pid").and_then(|p| p.as_u64()) else {
        return false;
    };
    pid_is_alive(pid)
}

#[cfg(windows)]
fn pid_is_alive(pid: u64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    let Ok(pid32) = u32::try_from(pid) else {
        return false;
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid32) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut exit_code = 0u32;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe { CloseHandle(handle) };
    !queried || exit_code == STILL_ACTIVE
}

const SPOOL_LAUNCHER_HARD_DEADLINE: Duration = Duration::from_secs(90);

pub fn claim_spool_launcher_slot(spool_dir: &Path) -> bool {
    static SLOT: OnceLock<Mutex<Option<fs::File>>> = OnceLock::new();
    let Ok(mut held) = SLOT.get_or_init(|| Mutex::new(None)).lock() else {
        return false;
    };
    if held.is_some() {
        return true;
    }
    let slot = spool_dir.join(".spool-launcher.pid");
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(slot)
    else {
        return false;
    };
    if file.try_lock_exclusive().is_err() {
        return false;
    }
    if file.set_len(0).is_err()
        || file
            .write_all(std::process::id().to_string().as_bytes())
            .is_err()
    {
        let _ = file.unlock();
        return false;
    }
    *held = Some(file);
    true
}

fn standalone_watcher_slot() -> &'static Mutex<Option<(PathBuf, fs::File)>> {
    static SLOT: OnceLock<Mutex<Option<(PathBuf, fs::File)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn claim_standalone_watcher_slot(spool_dir: &Path) -> bool {
    let Ok(mut held) = standalone_watcher_slot().lock() else {
        return false;
    };
    if let Some((held_spool_dir, _)) = held.as_ref() {
        return held_spool_dir == spool_dir;
    }
    let slot = spool_dir.join(".standalone-watcher.pid");
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(slot)
    else {
        return false;
    };
    if file.try_lock_exclusive().is_err() {
        return false;
    }
    if file.set_len(0).is_err()
        || file
            .write_all(std::process::id().to_string().as_bytes())
            .is_err()
    {
        let _ = file.unlock();
        return false;
    }
    *held = Some((spool_dir.to_path_buf(), file));
    true
}

pub fn release_standalone_watcher_slot(spool_dir: &Path) {
    let Ok(mut held) = standalone_watcher_slot().lock() else {
        return;
    };
    if held
        .as_ref()
        .is_some_and(|(held_spool_dir, _)| held_spool_dir == spool_dir)
    {
        *held = None;
    }
}

pub fn shared_daemon_is_serving() -> bool {
    shared_daemon_owner_that_would_refuse_this_process().is_some()
}

pub fn arm_spool_launcher_deadline() -> Arc<std::sync::atomic::AtomicBool> {
    let disarmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deadline_disarmed = disarmed.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SPOOL_LAUNCHER_HARD_DEADLINE);
        if deadline_disarmed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        eprintln!("[agentplug] spool launcher exceeded {}s without converging -- exiting so a wedged launch never lingers", SPOOL_LAUNCHER_HARD_DEADLINE.as_secs());
        std::process::exit(2);
    });
    disarmed
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u64) -> bool {
    let mut command = std::process::Command::new("kill");
    command
        .args(["-0", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.status().map(|s| s.success()).unwrap_or(true)
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
    if fs::metadata(&path)
        .map(|m| m.len() > DAEMON_LOG_MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, path.with_extension("log.prev"));
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
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

const TAKEOVER_READY_MAX_AGE_MS: u64 = 2 * 60 * 1000;

fn write_takeover_ready(version: &str) -> anyhow::Result<()> {
    let path = takeover_ready_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::json!({"version": version, "pid": std::process::id(), "ts": now_ms()})
            .to_string(),
    )?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn ready_takeover_successor() -> Option<(u64, String)> {
    let path = takeover_ready_path();
    let ready = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let Some(ready) = ready else {
        return None;
    };
    let pid = ready.get("pid").and_then(|value| value.as_u64());
    let version = ready
        .get("version")
        .and_then(|value| value.as_str())
        .filter(|version| !version.is_empty())
        .map(str::to_owned);
    let fresh = ready
        .get("ts")
        .and_then(|value| value.as_u64())
        .map(|ts| now_ms().saturating_sub(ts) <= TAKEOVER_READY_MAX_AGE_MS)
        .unwrap_or(false);
    match (pid, version, fresh) {
        (Some(pid), Some(version), true) if pid_is_alive(pid) => Some((pid, version)),
        _ => {
            let _ = fs::remove_file(path);
            None
        }
    }
}

fn hand_off_to_ready_successor() -> Option<(String, usize)> {
    let (successor_pid, version) = ready_takeover_successor()?;
    if !in_flight_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
    {
        return None;
    }
    let task_handoff = match agentplug_host::prepare_task_handoff() {
        Ok(Some(guard)) => guard,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("[agentplug daemon] task result preservation refuses ready-successor handoff: {error}");
            return None;
        }
    };
    if let Err(error) = reserve_ownership_for_handoff(successor_pid, &version) {
        eprintln!(
            "[agentplug daemon] ready takeover successor {version} cannot reserve the ownership handoff: {error}"
        );
        return None;
    }
    let claims = snapshot_in_flight_claims();
    write_handoff_inherited_claims(&version, &claims);
    release_ownership_for_handoff();
    task_handoff.commit();
    let requeued = requeue_claims_for_live_successor(&claims);
    mark_intentional_exit("ready-takeover");
    Some((version, requeued))
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
fn default_branch() -> String {
    "main".to_string()
}

fn instruction_source_config_path(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions").join("source.json")
}

fn instruction_source_cache_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions-source-cache")
}

fn spawn_pipe_drain_thread<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn run_git_bounded(args: &[&str]) -> anyhow::Result<std::process::Output> {
    use wait_timeout::ChildExt;
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    agentplug_host::apply_windowless(&mut cmd);
    let mut child = cmd.spawn()?;
    let timeout_ms = agentplug_host::git_subprocess_timeout_ms();
    let stdout_drain_thread = child.stdout.take().map(spawn_pipe_drain_thread);
    let stderr_drain_thread = child.stderr.take().map(spawn_pipe_drain_thread);
    match child.wait_timeout(Duration::from_millis(timeout_ms))? {
        Some(status) => {
            let stdout = stdout_drain_thread
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            let stderr = stderr_drain_thread
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            })
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
    let Ok(raw) = fs::read_to_string(&config_path) else {
        return Ok(());
    };
    let Ok(cfg) = serde_json::from_str::<InstructionSourceConfig>(&raw) else {
        eprintln!("[agentplug daemon] {} exists but does not parse as {{repo, branch?, path?}} -- ignoring", config_path.display());
        return Ok(());
    };
    let cache_dir = instruction_source_cache_dir(root);
    let cache_dir_str = cache_dir.to_string_lossy().into_owned();
    let git_dir_marker = cache_dir.join(".git");
    if !git_dir_marker.exists() {
        fs::create_dir_all(root.join(".gm"))?;
        let output = run_git_bounded(&[
            "clone",
            "--depth",
            "1",
            "--branch",
            &cfg.branch,
            &cfg.repo,
            &cache_dir_str,
        ])?;
        if !output.status.success() {
            anyhow::bail!("git clone of {} (branch {}) failed", cfg.repo, cfg.branch);
        }
        eprintln!(
            "[agentplug daemon] cloned instruction source {} (branch {}) for {}",
            cfg.repo,
            cfg.branch,
            root.display()
        );
        return Ok(());
    }
    let fetch = run_git_bounded(&[
        "-C",
        &cache_dir_str,
        "fetch",
        "--depth",
        "1",
        "origin",
        &cfg.branch,
    ])?;
    if !fetch.status.success() {
        anyhow::bail!("git fetch of {} (branch {}) failed", cfg.repo, cfg.branch);
    }
    let reset_target = format!("origin/{}", cfg.branch);
    let reset = run_git_bounded(&["-C", &cache_dir_str, "reset", "--hard", &reset_target])?;
    if !reset.status.success() {
        anyhow::bail!(
            "git reset of instruction source cache for {} failed",
            root.display()
        );
    }
    Ok(())
}

fn staged_version_output_matches(text: &str, expected_version: &str) -> bool {
    text.trim() == format!("agentplug-runner {expected_version}")
}

#[cfg(test)]
mod staged_binary_self_check_tests {
    use super::staged_version_output_matches;

    #[test]
    fn version_output_requires_the_exact_runner_identity_and_version() {
        assert!(staged_version_output_matches(
            "agentplug-runner 1.2.3\n",
            "1.2.3"
        ));
        assert!(!staged_version_output_matches(
            "other-runner 1.2.3",
            "1.2.3"
        ));
        assert!(!staged_version_output_matches(
            "agentplug-runner 1.2.3-dev",
            "1.2.3"
        ));
        assert!(!staged_version_output_matches(
            "agentplug-runner 1.2.30",
            "1.2.3"
        ));
    }
}

fn staged_binary_self_check(staged_exe: &Path, expected_version: &str) -> bool {
    let mut cmd = std::process::Command::new(staged_exe);
    cmd.arg("--version");
    agentplug_host::apply_windowless(&mut cmd);
    let output = cmd.output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            if staged_version_output_matches(&text, expected_version) {
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
                staged_exe.display(),
                out.status
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
        record_handoff_failure(
            version,
            format!("staged_binary_self_check failed for {version}, staged exe removed"),
        );
        return false;
    }
    let ready_path = takeover_ready_path();
    let _ = fs::remove_file(&ready_path);
    if let Err(e) = spawn_detached(staged_exe, &["takeover", version]) {
        record_handoff_failure(
            version,
            format!("spawn_detached of staged {version} failed: {e}"),
        );
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(raw) = fs::read_to_string(&ready_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v.get("version").and_then(|x| x.as_str()) == Some(version) {
                    let Some(successor_pid) = v.get("pid").and_then(|x| x.as_u64()) else {
                        continue;
                    };
                    if let Err(error) = reserve_ownership_for_handoff(successor_pid, version) {
                        eprintln!(
                            "[agentplug daemon] self-update successor for {version} cannot reserve the ownership handoff: {error}"
                        );
                        continue;
                    }
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
    record_handoff_failure(
        version,
        format!("staged {version} did not write a matching readiness marker within 10s"),
    );
    false
}

fn release_ownership_for_handoff() {
    let my_pid = std::process::id() as u64;
    if read_owner_pid() == Some(my_pid) {
        let _ = fs::remove_file(daemon_owner_path());
    }
}

fn path_is_cargo_build_output(path: &Path) -> bool {
    path.ancestors().any(|dir| {
        dir.file_name().and_then(|n| n.to_str()) == Some("target")
            && dir
                .parent()
                .map(|p| p.join("Cargo.toml").exists())
                .unwrap_or(false)
    })
}

fn promote_staged_exe_to_canonical(version: &str) -> bool {
    let Some(canonical) = canonical_runner_exe_path() else {
        return false;
    };
    if path_is_cargo_build_output(&canonical) {
        eprintln!(
            "[agentplug daemon] takeover: refusing to promote {version} onto {} -- that path is a cargo build output (a target/ dir beside a Cargo.toml), not an installed runner; this process keeps running from the staged copy instead of overwriting the build artifact",
            canonical.display()
        );
        return false;
    }
    if let Some(reason) = crate::download::installed_runner_blocks_promotion(&canonical) {
        eprintln!(
            "[agentplug daemon] takeover: refusing to promote {version} onto {} -- {reason}; this process keeps running from the staged copy instead of overwriting it",
            canonical.display()
        );
        return false;
    }
    let Ok(staged) = std::env::current_exe() else {
        return false;
    };
    if staged == canonical {
        return false;
    }
    let prev = canonical.with_extension(
        canonical
            .extension()
            .map(|e| format!("{}.prev", e.to_string_lossy()))
            .unwrap_or_else(|| "prev".to_string()),
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
    let replacement = canonical.with_extension(format!(
        "{}.replace.{}",
        canonical
            .extension()
            .map(|extension| extension.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    let promotion = (|| -> std::io::Result<()> {
        fs::copy(&staged, &replacement)?;
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&replacement)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&replacement, perms)?;
        }
        fs::rename(&replacement, &canonical)
    })();
    match promotion {
        Ok(()) => {
            record_completed_runner_swap(version);
            eprintln!("[agentplug daemon] takeover: promoted {version} onto canonical exe path {} (previous version kept at {})", canonical.display(), prev.display());
            true
        }
        Err(e) => {
            let _ = fs::remove_file(&replacement);
            eprintln!(
                "[agentplug daemon] takeover: failed to atomically promote staged exe onto canonical path {}: {e} -- restoring previous version at canonical path",
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
        Some(log) => {
            cmd.stderr(std::process::Stdio::from(log));
        }
        None => {
            cmd.stderr(std::process::Stdio::null());
        }
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
    for plugin_name in ["gm", "bert", "libsql", "treesitter", "crux", "lightpanda"] {
        if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
            eprintln!("[agentplug daemon] takeover: pre-warm of {plugin_name} failed (non-fatal, will lazy-compile on first use): {e}");
        }
    }
    write_takeover_ready(version)?;
    eprintln!("[agentplug daemon] takeover: readiness marker written, waiting for old daemon to release ownership");
    for _ in 0..480 {
        if read_owner_pid().is_none() && claim_ownership() {
            let _ = fs::remove_file(takeover_ready_path());
            if let Err(error) = record_runner_version(version) {
                eprintln!(
                    "[agentplug daemon] takeover: could not record running version {version}: {error} -- continuing with the staged runner so the old daemon is not left without a successor; a later boot will reconcile the marker"
                );
            } else {
                crate::download::clear_all_known_bad_version_markers();
            }
            let promoted = promote_staged_exe_to_canonical(version);
            if promoted {
                if let Some(canonical) = canonical_runner_exe_path() {
                    release_ownership_for_handoff();
                    reexec_from_canonical_and_exit(&canonical);
                }
            }
            eprintln!(
                "[agentplug daemon] takeover: ownership claimed, entering normal daemon loop"
            );
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
            if hashes.is_empty() {
                None
            } else {
                Some((name.to_string(), serde_json::json!(hashes)))
            }
        })
        .collect()
}

fn write_daemon_heartbeat(project_count: usize, plugin_module_count: usize) {
    static NEXT_HEARTBEAT_WRITE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let last_plugin_poll_ts =
        HEARTBEAT_LAST_PLUGIN_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let last_runner_poll_ts =
        HEARTBEAT_LAST_RUNNER_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let loaded_content_hashes: HashMap<String, String> = loaded_plugin_content_hashes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let shared_pool_slots: HashMap<String, Vec<agentplug_host::SlotContentSnapshot>> =
        ["gm", "bert", "libsql", "treesitter"]
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    agentplug_host::shared_plugin_slot_snapshot_without_blocking(name),
                )
            })
            .collect();
    let mixed_version_pools: Vec<String> = shared_pool_slots
        .iter()
        .filter(|(_, slots)| {
            slots
                .iter()
                .filter_map(|s| match s {
                    agentplug_host::SlotContentSnapshot::Loaded { content_hash } => {
                        Some(content_hash)
                    }
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
                    agentplug_host::SlotContentSnapshot::Loaded { content_hash } => {
                        serde_json::json!(content_hash)
                    }
                    agentplug_host::SlotContentSnapshot::BusyWithDispatchInFlight => {
                        serde_json::json!("busy-dispatch-in-flight")
                    }
                })
                .collect();
            (name, rendered)
        })
        .collect();
    let boot_ts = HEARTBEAT_DAEMON_BOOT_TS.load(std::sync::atomic::Ordering::Relaxed);
    let plugin_poll_error = last_plugin_poll_error()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let runner_poll_error = last_runner_poll_error()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let staged_runner = refresh_staged_runner_cache();
    let handoff_attempt = last_handoff_attempt()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let plugin_compile_failures: HashMap<String, String> = last_plugin_compile_failure()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let status_path = daemon_status_path();
    let payload = serde_json::json!({
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
            "runner_version_parity": runner_version_parity_json(),
    })
    .to_string();
    let temp_path = status_path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        NEXT_HEARTBEAT_WRITE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    if fs::write(&temp_path, payload).is_ok() && fs::rename(&temp_path, &status_path).is_ok() {
        return;
    }
    let _ = fs::remove_file(temp_path);
}

fn last_completed_runner_swap_path() -> PathBuf {
    install_dir().join("last-completed-runner-swap.json")
}

fn record_completed_runner_swap(version: &str) {
    let sha256 = canonical_runner_exe_path()
        .and_then(|p| fs::read(p).ok())
        .map(|bytes| crate::download::sha256_hex(&bytes));
    let mut record = serde_json::json!({ "version": version, "swapped_at_ts": now_ms() });
    if let Some(sha256) = sha256 {
        record["sha256"] = serde_json::json!(sha256);
    }
    let _ = fs::write(last_completed_runner_swap_path(), record.to_string());
}

fn read_last_completed_runner_swap() -> Option<serde_json::Value> {
    let text = fs::read_to_string(last_completed_runner_swap_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_recorded_swap_field(field: &str) -> Option<String> {
    read_last_completed_runner_swap()?
        .get(field)?
        .as_str()
        .map(|s| s.to_string())
}

struct RunnerVersionParity {
    exe: Option<String>,
    sha256: Option<String>,
    exe_reported_version: Option<String>,
    compiled_version: String,
    installed_version_file: Option<String>,
    recorded_swap_version: Option<String>,
    recorded_swap_sha256: Option<String>,
    pinned_local_build_sha256: Option<String>,
}

fn exe_reported_runner_version(exe: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    agentplug_host::apply_windowless(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_start_matches('v')
        .to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn probe_runner_version_parity() -> RunnerVersionParity {
    let exe = canonical_runner_exe_path();
    let sha256 = exe
        .as_ref()
        .and_then(|p| fs::read(p).ok())
        .map(|b| crate::download::sha256_hex(&b));
    let pinned_local_build_sha256 = crate::download::local_build_pin_record()
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    RunnerVersionParity {
        exe: exe.as_ref().map(|p| p.display().to_string()),
        sha256,
        exe_reported_version: exe.as_ref().and_then(|p| exe_reported_runner_version(p)),
        compiled_version: env!("CARGO_PKG_VERSION").to_string(),
        installed_version_file: installed_runner_version(),
        recorded_swap_version: read_recorded_swap_field("version"),
        recorded_swap_sha256: read_recorded_swap_field("sha256"),
        pinned_local_build_sha256,
    }
}

impl RunnerVersionParity {
    fn live_sha256(&self) -> &str {
        self.sha256.as_deref().unwrap_or("unreadable")
    }

    fn sha_matches(&self, candidate: Option<&String>) -> bool {
        candidate
            .map(|c| c.eq_ignore_ascii_case(self.live_sha256()))
            .unwrap_or(false)
    }

    fn installed_by_recorded_swap(&self) -> bool {
        self.sha_matches(self.recorded_swap_sha256.as_ref())
    }

    fn installed_by_local_build_pin(&self) -> bool {
        self.sha_matches(self.pinned_local_build_sha256.as_ref())
    }

    fn disagreements(&self) -> Vec<String> {
        let mut out = Vec::new();
        let live = self.live_sha256();
        if !self.installed_by_recorded_swap() && !self.installed_by_local_build_pin() {
            if self.recorded_swap_sha256.is_none() && self.pinned_local_build_sha256.is_none() {
                out.push(format!(
                    "no completed runner swap and no local-build pin names which bytes should be live, so this daemon runs sha256 {live} on trust alone"
                ));
            } else {
                out.push(format!(
                    "this daemon runs sha256 {live} but the last completed runner swap names sha256 {} and the local-build pin names sha256 {} -- the live bytes were installed by neither",
                    self.recorded_swap_sha256.as_deref().unwrap_or("none"),
                    self.pinned_local_build_sha256.as_deref().unwrap_or("none")
                ));
            }
        }
        if let Some(reported) = self.exe_reported_version.as_deref() {
            if reported != self.compiled_version {
                out.push(format!(
                    "the runner exe at {} reports --version {reported} but this daemon was compiled as {} -- the exe on disk was replaced after this process started",
                    self.exe.as_deref().unwrap_or("an unresolvable path"),
                    self.compiled_version
                ));
            }
        }
        if let Some(installed) = self.installed_version_file.as_deref() {
            if installed != self.compiled_version {
                out.push(format!(
                    "{} records {installed} but this daemon was compiled as {} -- the version file and the live binary disagree",
                    crate::download::runner_version_path().display(),
                    self.compiled_version
                ));
            }
        }
        if let Some(swap) = self.recorded_swap_version.as_deref() {
            if swap != self.compiled_version && !self.installed_by_local_build_pin() {
                out.push(format!(
                    "the last completed runner swap records version {swap} but this daemon was compiled as {} and is not the pinned local build -- the bytes that are meant to be live are not the bytes running",
                    self.compiled_version
                ));
            }
        }
        out
    }

    fn json(&self) -> serde_json::Value {
        let disagreements = self.disagreements();
        serde_json::json!({
            "agrees": disagreements.is_empty(),
            "exe": self.exe,
            "exe_sha256": self.sha256,
            "exe_reported_version": self.exe_reported_version,
            "compiled_version": self.compiled_version,
            "installed_version_file": self.installed_version_file,
            "recorded_swap_version": self.recorded_swap_version,
            "recorded_swap_sha256": self.recorded_swap_sha256,
            "pinned_local_build_sha256": self.pinned_local_build_sha256,
            "disagreements": disagreements,
        })
    }
}

fn runner_version_parity() -> &'static RunnerVersionParity {
    static SLOT: OnceLock<RunnerVersionParity> = OnceLock::new();
    SLOT.get_or_init(probe_runner_version_parity)
}

fn runner_version_parity_json() -> serde_json::Value {
    runner_version_parity().json()
}

fn canonical_runner_exe_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    while path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("new"))
        .unwrap_or(false)
    {
        path = path.with_extension("");
    }
    Some(path)
}

fn staged_matches_running(canonical: &Path, staged: &Path) -> bool {
    let Ok(running_meta) = fs::metadata(canonical) else {
        return false;
    };
    let Ok(staged_meta) = fs::metadata(staged) else {
        return false;
    };
    if running_meta.len() != staged_meta.len() {
        return false;
    }
    let Ok(running_bytes) = fs::read(canonical) else {
        return false;
    };
    let Ok(staged_bytes) = fs::read(staged) else {
        return false;
    };
    running_bytes == staged_bytes
}

fn staged_runner_awaiting_handoff() -> Option<(u64, u64)> {
    let canonical = canonical_runner_exe_path()?;
    let staged = canonical.with_extension(
        canonical
            .extension()
            .map(|e| format!("{}.new", e.to_string_lossy()))
            .unwrap_or_else(|| "new".to_string()),
    );
    if std::env::current_exe().ok().as_deref() == Some(staged.as_path()) {
        return None;
    }
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
    *staged_runner_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = value;
    value
}

fn cached_staged_runner() -> Option<(u64, u64)> {
    *staged_runner_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn write_project_heartbeat(root: &Path, busy_until: Option<u64>) {
    write_project_heartbeat_with_queue_info(root, busy_until, None);
}

const DISPATCH_HEARTBEAT_MIN_GAP: Duration = Duration::from_secs(3);

fn dispatch_heartbeat_last_write() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn write_project_heartbeat_rate_limited(
    root: &Path,
    busy_until: Option<u64>,
    queue_info: Option<(usize, usize)>,
) {
    let now = Instant::now();
    {
        let mut last_writes = dispatch_heartbeat_last_write()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(at) = last_writes.get(root) {
            if now.saturating_duration_since(*at) < DISPATCH_HEARTBEAT_MIN_GAP {
                return;
            }
        }
        last_writes.insert(root.to_path_buf(), now);
    }
    write_project_heartbeat_with_queue_info(root, busy_until, queue_info);
}

fn write_project_heartbeat_with_queue_info(
    root: &Path,
    busy_until: Option<u64>,
    queue_info: Option<(usize, usize)>,
) {
    let spool_dir = spool_dir_of(root);
    let status_path = spool_dir.join(".status.json");
    let mut payload = match fs::read_to_string(&status_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => serde_json::json!({}),
    };
    let previous = payload.clone();
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
    let (queued_steps, claimed_steps) = spool_step_counts(root);
    payload["queued_step_count"] = serde_json::json!(queued_steps);
    payload["claimed_step_count"] = serde_json::json!(claimed_steps);
    payload["gm_processor_capacity"] =
        serde_json::json!(GM_PROCESSOR_CAPACITY.load(std::sync::atomic::Ordering::Relaxed));
    payload["gm_processor_capacity_reason"] = serde_json::json!(gm_processor_capacity_reason()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone());
    payload["shared_store_recycle_limit_mb"] =
        serde_json::json!(SHARED_STORE_RECYCLE_LIMIT_MB.load(std::sync::atomic::Ordering::Relaxed));
    payload["tool_serialization"] = serde_json::json!("fifo per plugin and verb for state-changing verbs, one dispatch per project lane (git, read, store, state); exec-family, read-only verbs and tree-scan codesearch run unserialised");
    payload["runner_version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));
    payload["loaded_plugin_versions"] = serde_json::json!(loaded_plugin_versions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone());
    payload["queue_wait_ms"] = serde_json::json!(last_measured_dispatch_queue_wait_ms());
    if let Some((staged_at_ms, _len)) = cached_staged_runner() {
        payload["runner_update_in_progress"] = serde_json::json!(true);
        payload["runner_update_waiting_ms"] =
            serde_json::json!(now_ms().saturating_sub(staged_at_ms));
    } else if let Some(map) = payload.as_object_mut() {
        map.remove("runner_update_in_progress");
        map.remove("runner_update_waiting_ms");
    }
    match sweep_holder_report() {
        Some((phase, root, held_ms)) => {
            payload["sweep_holder_phase"] = serde_json::json!(phase);
            payload["sweep_holder_root"] = serde_json::json!(root);
            payload["sweep_holder_ms"] = serde_json::json!(held_ms);
        }
        None => {
            if let Some(map) = payload.as_object_mut() {
                map.remove("sweep_holder_phase");
                map.remove("sweep_holder_root");
                map.remove("sweep_holder_ms");
            }
        }
    }
    let failures = last_plugin_compile_failure()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if failures.is_empty() {
        payload
            .as_object_mut()
            .map(|m| m.remove("plugin_compile_failures"));
    } else {
        payload["plugin_compile_failures"] = serde_json::json!(failures);
    }
    if !status_write_needed(&status_path, &previous, &payload) {
        return;
    }
    let _ = fs::write(&status_path, payload.to_string());
}

const STATUS_LIVENESS_REFRESH: Duration = Duration::from_secs(600);

fn status_write_needed(
    status_path: &Path,
    previous: &serde_json::Value,
    next: &serde_json::Value,
) -> bool {
    fn without_volatile_fields(value: &serde_json::Value) -> serde_json::Value {
        let mut copy = value.clone();
        if let Some(map) = copy.as_object_mut() {
            for key in [
                "ts",
                "sweep_holder_ms",
                "sweep_holder_phase",
                "sweep_holder_root",
                "runner_update_waiting_ms",
                "queue_wait_ms",
            ] {
                map.remove(key);
            }
        }
        copy
    }
    if without_volatile_fields(previous) != without_volatile_fields(next) {
        return true;
    }
    let last_written_age = fs::metadata(status_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    match last_written_age {
        Some(age) => age >= STATUS_LIVENESS_REFRESH,
        None => true,
    }
}

fn known_project_roots() -> &'static Mutex<Vec<PathBuf>> {
    static SLOT: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_known_project_roots(roots: &[PathBuf]) {
    *known_project_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = roots.to_vec();
}

pub fn read_known_project_roots() -> Vec<PathBuf> {
    known_project_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

const IDLE_PROJECT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const IDLE_PROJECT_SCAN_INTERVAL: Duration = Duration::from_secs(600);

fn project_scan_state() -> &'static Mutex<HashMap<PathBuf, (Instant, bool)>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn project_scan_due(root: &Path) -> bool {
    let state = project_scan_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match state.get(root) {
        Some((at, had_work)) => *had_work || at.elapsed() >= IDLE_PROJECT_SCAN_INTERVAL,
        None => true,
    }
}

fn record_project_scan(root: &Path, has_work: bool) {
    project_scan_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root.to_path_buf(), (Instant::now(), has_work));
}

fn project_heartbeat_last_write() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn project_heartbeat_due(root: &Path, has_queued_work: bool) -> bool {
    let now = Instant::now();
    let mut last_writes = project_heartbeat_last_write()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let due = has_queued_work
        || match last_writes.get(root) {
            Some(at) => now.saturating_duration_since(*at) >= IDLE_PROJECT_HEARTBEAT_INTERVAL,
            None => true,
        };
    if due {
        last_writes.insert(root.to_path_buf(), now);
    }
    due
}

struct SweepHolder {
    phase: &'static str,
    root: String,
    since: Instant,
}

fn sweep_holder() -> &'static Mutex<Option<SweepHolder>> {
    static SLOT: OnceLock<Mutex<Option<SweepHolder>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn set_sweep_holder(phase: &'static str, root: &Path) {
    if let Ok(mut slot) = sweep_holder().lock() {
        *slot = Some(SweepHolder {
            phase,
            root: root.display().to_string(),
            since: Instant::now(),
        });
    }
}

fn clear_sweep_holder() {
    if let Ok(mut slot) = sweep_holder().lock() {
        *slot = None;
    }
}

fn sweep_holder_report() -> Option<(String, String, u64)> {
    sweep_holder().lock().ok().and_then(|slot| {
        slot.as_ref()
            .map(|h| (h.phase.to_string(), h.root.clone(), h.since.elapsed().as_millis() as u64))
    })
}

struct RegistryWalk {
    cursor: usize,
}

impl RegistryWalk {
    fn reset(&mut self) {
        self.cursor = 0;
    }

    fn walk<F: FnMut(&PathBuf)>(
        &mut self,
        roots: &[PathBuf],
        budget: Duration,
        phase: &'static str,
        mut body: F,
    ) -> bool {
        if roots.is_empty() {
            return true;
        }
        let started = Instant::now();
        let begin = self.cursor % roots.len();
        let mut next = begin;
        let mut completed = true;
        for offset in 0..roots.len() {
            let index = (begin + offset) % roots.len();
            let root = &roots[index];
            next = (index + 1) % roots.len();
            set_sweep_holder(phase, root);
            body(root);
            if started.elapsed() >= budget {
                completed = false;
                break;
            }
        }
        self.cursor = next;
        clear_sweep_holder();
        completed
    }
}

fn project_heartbeat_cursor() -> &'static Mutex<usize> {
    static SLOT: OnceLock<Mutex<usize>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(0))
}

const PROJECT_HEARTBEAT_PASS_BUDGET: Duration = Duration::from_millis(4_000);

fn spawn_project_heartbeat_ticker(interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        if heartbeat_authority_lost() {
            return;
        }
        let roots = known_project_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if roots.is_empty() {
            continue;
        }
        let started = Instant::now();
        let begin = *project_heartbeat_cursor()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut next = begin;
        for offset in 0..roots.len() {
            let index = (begin + offset) % roots.len();
            let root = &roots[index];
            next = (index + 1) % roots.len();
            if !project_scan_due(root) {
                continue;
            }
            let spool_dir = spool_dir_of(root);
            if !spool_dir.exists() {
                continue;
            }
            set_sweep_holder("project-heartbeat", root);
            let (queued, claimed) = spool_step_counts(root);
            record_project_scan(root, queued + claimed > 0);
            if !project_heartbeat_due(root, queued + claimed > 0) {
                continue;
            }
            write_project_heartbeat(root, busy_until_for_project_ticker(root));
            if started.elapsed() >= PROJECT_HEARTBEAT_PASS_BUDGET {
                break;
            }
        }
        *project_heartbeat_cursor()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = next;
        clear_sweep_holder();
    })
}

fn spawn_dream_rsi_cycle_ticker(interval: Duration) -> std::thread::JoinHandle<()> {
    crate::dream_cycle::spawn(interval, read_known_project_roots, heartbeat_authority_lost)
}

fn spool_dir_of(root: &Path) -> PathBuf {
    root.join(".gm").join("exec-spool")
}

fn read_status_busy_until_if_future(root: &Path) -> Option<u64> {
    let text = fs::read_to_string(spool_dir_of(root).join(".status.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let busy_until = value.get("busy_until")?.as_u64()?;
    (busy_until > now_ms()).then_some(busy_until)
}

const TICKER_BUSY_UNTIL_EXTEND_MS: u64 = 60_000;

fn busy_until_for_project_ticker(root: &Path) -> Option<u64> {
    if project_in_flight_count(root) > 0 || spool_has_queued_work(root) {
        let stored = read_status_busy_until_if_future(root);
        let refresh_below = now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS / 2;
        return match stored {
            Some(until) if until > refresh_below => Some(until),
            _ => Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS),
        };
    }
    read_status_busy_until_if_future(root)
}

const SPOOL_SCAN_BACKSTOP_TTL: Duration = Duration::from_secs(30);

struct RootScan {
    at: Instant,
    claimable: bool,
    has_queued_work: bool,
    queued_steps: usize,
    claimed_steps: usize,
}

fn spool_dirty_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static SLOT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn mark_spool_dirty(root: &Path) {
    if let Ok(mut dirty) = spool_dirty_roots().lock() {
        dirty.insert(root.to_path_buf());
    }
}

fn root_scan_cache() -> &'static Mutex<HashMap<PathBuf, RootScan>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, RootScan>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn count_spool_steps(root: &Path) -> (bool, usize, usize) {
    let mut queued = 0usize;
    let mut claimed = 0usize;
    let mut has_queued_work = false;
    let in_dir = spool_dir_of(root).join("in");
    let Ok(verbs) = fs::read_dir(&in_dir) else {
        return (has_queued_work, queued, claimed);
    };
    for verb_entry in verbs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let name = file_entry.file_name().to_string_lossy().into_owned();
            if is_spool_request_path(&verb, &file_entry.path()) {
                queued += 1;
                has_queued_work = true;
            } else if name.ends_with(".inflight") {
                claimed += 1;
                has_queued_work = true;
            }
        }
    }
    (has_queued_work, queued, claimed)
}

fn cached_root_scan(root: &Path) -> (bool, bool, usize, usize) {
    let now = Instant::now();
    let dirty = spool_dirty_roots()
        .lock()
        .map(|d| d.contains(root))
        .unwrap_or(true);
    if !dirty {
        let cached = root_scan_cache().lock().ok().and_then(|cache| {
            cache.get(root).map(|s| {
                (
                    s.at,
                    s.claimable,
                    s.has_queued_work,
                    s.queued_steps,
                    s.claimed_steps,
                )
            })
        });
        if let Some((at, claimable, has_queued_work, queued_steps, claimed_steps)) = cached {
            if now.saturating_duration_since(at) < SPOOL_SCAN_BACKSTOP_TTL {
                return (claimable, has_queued_work, queued_steps, claimed_steps);
            }
        }
    }
    let queued_work = project_has_queued_spool_work(root);
    let counted = count_spool_steps(root);
    let scan = RootScan {
        at: Instant::now(),
        claimable: queued_work,
        has_queued_work: counted.0,
        queued_steps: counted.1,
        claimed_steps: counted.2,
    };
    let result = (
        scan.claimable,
        scan.has_queued_work,
        scan.queued_steps,
        scan.claimed_steps,
    );
    if let Ok(mut cache) = root_scan_cache().lock() {
        cache.insert(root.to_path_buf(), scan);
    }
    if let Ok(mut dirty) = spool_dirty_roots().lock() {
        dirty.remove(root);
    }
    result
}

fn spool_has_queued_work(root: &Path) -> bool {
    cached_root_scan(root).1
}

fn spool_step_counts(root: &Path) -> (usize, usize) {
    let (_, _, queued, claimed) = cached_root_scan(root);
    (queued, claimed)
}

static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_LAST_PLUGIN_POLL_TS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_LAST_RUNNER_POLL_TS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_DAEMON_BOOT_TS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static GM_PROCESSOR_CAPACITY: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(1);
static SHARED_STORE_RECYCLE_LIMIT_MB: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
    *last_plugin_poll_error()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = err;
}

fn record_runner_poll_error(err: Option<String>) {
    *last_runner_poll_error()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = err;
}

type HandoffAttempt = (u64, Option<String>);

fn last_handoff_attempt() -> &'static Mutex<Option<HandoffAttempt>> {
    static SLOT: OnceLock<Mutex<Option<HandoffAttempt>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_handoff_attempt(error: Option<String>) {
    *last_handoff_attempt()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((now_ms(), error));
}

fn consecutive_handoff_failures() -> &'static Mutex<(String, u32)> {
    static SLOT: OnceLock<Mutex<(String, u32)>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new((String::new(), 0)))
}

const HANDOFF_ESCALATION_THRESHOLD: u32 = 3;

const HANDOFF_RETRY_BACKOFF: Duration = Duration::from_secs(30 * 60);

fn handoff_retry_backoff_slot() -> &'static Mutex<Option<(String, Instant)>> {
    static SLOT: OnceLock<Mutex<Option<(String, Instant)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn handoff_backed_off(version: &str) -> bool {
    let slot = handoff_retry_backoff_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some((blocked, until)) => blocked == version && Instant::now() < *until,
        None => false,
    }
}

fn record_handoff_backoff(version: &str) {
    *handoff_retry_backoff_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        Some((version.to_string(), Instant::now() + HANDOFF_RETRY_BACKOFF));
}

fn clear_handoff_backoff(version: &str) {
    let mut slot = handoff_retry_backoff_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().map(|(v, _)| v.as_str()) == Some(version) {
        *slot = None;
    }
}

fn runner_update_escalation_path() -> PathBuf {
    install_dir().join("runner-update-escalation.json")
}

pub(crate) fn patch_update_available_from_escalation(
    plugin: &str,
    verb: &str,
    response: String,
) -> String {
    if plugin != "gm" || verb != "instruction" {
        return response;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&response) else {
        return response;
    };
    let Some(obj) = value.as_object_mut() else {
        return response;
    };
    if !matches!(
        obj.get("update_available"),
        Some(serde_json::Value::Null) | None
    ) {
        return response;
    }
    let Ok(marker_raw) = fs::read_to_string(runner_update_escalation_path()) else {
        return response;
    };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&marker_raw) else {
        return response;
    };
    obj.insert("update_available".to_string(), marker);
    value.to_string()
}

fn record_handoff_failure(version: &str, reason: String) {
    record_handoff_attempt(Some(reason.clone()));
    record_handoff_backoff(version);
    let mut slot = consecutive_handoff_failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.0 != version {
        *slot = (version.to_string(), 1);
    } else {
        slot.1 += 1;
    }
    if slot.1 != HANDOFF_ESCALATION_THRESHOLD {
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
    let mut slot = consecutive_handoff_failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.0 == version {
        *slot = (String::new(), 0);
    }
    clear_handoff_backoff(version);
    let _ = fs::remove_file(runner_update_escalation_path());
}

fn persisted_plugin_poll_ts_path() -> PathBuf {
    install_dir().join("last-plugin-update-poll-ts")
}

fn persisted_runner_poll_ts_path() -> PathBuf {
    install_dir().join("last-runner-update-poll-ts")
}

fn read_persisted_poll_ts(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
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
    let elapsed_ms = if persisted_ts == 0 {
        NEVER_POLLED_BACKDATE_MS
    } else {
        now_ms().saturating_sub(persisted_ts)
    };
    instant_backdated_by_ms_capped_to_process_epoch(elapsed_ms)
}
static LOADED_PLUGIN_CONTENT_HASHES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn loaded_plugin_content_hashes() -> &'static Mutex<HashMap<String, String>> {
    LOADED_PLUGIN_CONTENT_HASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn loaded_plugin_versions() -> &'static Mutex<HashMap<String, String>> {
    static LOADED_PLUGIN_VERSIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    LOADED_PLUGIN_VERSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

static LAST_PLUGIN_COMPILE_FAILURE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn last_plugin_compile_failure() -> &'static Mutex<HashMap<String, String>> {
    LAST_PLUGIN_COMPILE_FAILURE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_plugin_compile_failure(plugin_name: &str, reason: String) {
    last_plugin_compile_failure()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(plugin_name.to_string(), reason);
    plugin_compile_backoff_until()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            plugin_name.to_string(),
            Instant::now() + PLUGIN_COMPILE_RETRY_BACKOFF,
        );
}

fn clear_plugin_compile_failure(plugin_name: &str) {
    last_plugin_compile_failure()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(plugin_name);
    plugin_compile_backoff_until()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(plugin_name);
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
    last_plugin_compile_failure()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(plugin_name)
        .cloned()
}

static HEARTBEAT_AUTHORITY_LOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    recompiling: HashMap<
        String,
        (
            String,
            Mutex<std::sync::mpsc::Receiver<Result<Module, String>>>,
        ),
    >,
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
            recompiling: HashMap::new(),
        })
    }

    fn install_compiled(
        &mut self,
        plugin_name: &str,
        module: Module,
        on_disk_hash: String,
        wasm_path: &Path,
    ) {
        if let Some(old_hash) = self.loaded_content_hash.get(plugin_name).cloned() {
            if old_hash != on_disk_hash {
                let (evicted_now, deferred) =
                    agentplug_host::request_shared_store_swap(plugin_name, &old_hash);
                eprintln!(
                    "[agentplug daemon] {plugin_name}.wasm recompiled off the claim loop -- swapping it in, draining the shared Stores on the old module ({evicted_now} slot(s) evicted now, {deferred} still in-flight and finishing on the old Store; their slots evict on completion)"
                );
            }
        }
        if let Some(installed) = installed_plugin_version(plugin_name) {
            if !is_recognized_release_semver(&installed) {
                eprintln!(
                    "[agentplug daemon] BOOT WARNING: {plugin_name}.wasm at {} is served from a NON-RELEASE version marker ({installed:?}) -- this is a local-dev sideload, not a released build, and the auto-updater will never overwrite it. If this was not intentional, replace the sideload with a real release-tagged {plugin_name}.wasm.",
                    wasm_path.display()
                );
            }
        }
        self.modules.insert(plugin_name.to_string(), module);
        self.loaded_content_hash
            .insert(plugin_name.to_string(), on_disk_hash.clone());
        loaded_plugin_content_hashes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plugin_name.to_string(), on_disk_hash.clone());
        loaded_plugin_versions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                plugin_name.to_string(),
                installed_plugin_version(plugin_name).unwrap_or_else(|| "unversioned".to_string()),
            );
        agentplug_host::note_shared_plugin_bytes_current(plugin_name, &on_disk_hash);
    }

    fn adopt_finished_recompile(
        &mut self,
        plugin_name: &str,
        wasm_path: &Path,
    ) -> anyhow::Result<()> {
        let Some((hash, receiver)) = self.recompiling.remove(plugin_name) else {
            return Ok(());
        };
        let received = receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv();
        match received {
            Ok(Ok(module)) => {
                self.install_compiled(plugin_name, module, hash, wasm_path);
                Ok(())
            }
            Ok(Err(e)) => {
                self.last_hash_check_stat.remove(plugin_name);
                Err(anyhow::anyhow!(
                    "background recompile of {plugin_name} failed: {e}"
                ))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.recompiling
                    .insert(plugin_name.to_string(), (hash, receiver));
                Ok(())
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.last_hash_check_stat.remove(plugin_name);
                anyhow::bail!(
                    "background recompile thread for {plugin_name} ended without a result"
                )
            }
        }
    }

    fn get_or_compile(&mut self, plugin_name: &str) -> anyhow::Result<()> {
        let wasm_path = ensure_plugin_installed(plugin_name, None)?;
        self.adopt_finished_recompile(plugin_name, &wasm_path)?;
        let current_stat = wasm_file_stat(&wasm_path);
        let stat_unchanged = current_stat.is_some()
            && current_stat == self.last_hash_check_stat.get(plugin_name).copied();
        if stat_unchanged && self.modules.contains_key(plugin_name) {
            return Ok(());
        }
        let on_disk_hash = wasm_file_content_hash(&wasm_path)?;
        if let Some(stat) = current_stat {
            self.last_hash_check_stat
                .insert(plugin_name.to_string(), stat);
        }
        let stale = self
            .loaded_content_hash
            .get(plugin_name)
            .is_some_and(|loaded_hash| loaded_hash != &on_disk_hash);
        if self.modules.contains_key(plugin_name) && !stale {
            self.recompiling.remove(plugin_name);
            return Ok(());
        }
        if stale {
            if self
                .recompiling
                .get(plugin_name)
                .is_some_and(|(pending_hash, _)| pending_hash == &on_disk_hash)
            {
                return Ok(());
            }
            eprintln!("[agentplug daemon] {plugin_name}.wasm content hash changed on disk since it was last compiled -- recompiling on a background thread while the loaded module keeps serving");
            let (tx, rx) = std::sync::mpsc::channel();
            let engine = self.engine.clone();
            let path = wasm_path.clone();
            let thread_plugin_name = plugin_name.to_string();
            let thread_hash = on_disk_hash.clone();
            let thread_name = format!("recompile-{plugin_name}");
            let spawned = std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    let _ = tx.send(
                        agentplug_host::load_module_file_backed(
                            &engine,
                            &path,
                            &thread_plugin_name,
                            &thread_hash,
                        )
                        .map_err(|e| format!("{e:#}")),
                    );
                });
            if let Err(e) = spawned {
                self.last_hash_check_stat.remove(plugin_name);
                anyhow::bail!(
                    "could not start the background recompile thread for {plugin_name}: {e}"
                );
            }
            self.recompiling
                .insert(plugin_name.to_string(), (on_disk_hash, Mutex::new(rx)));
            return Ok(());
        }
        eprintln!("[agentplug daemon] compiling {plugin_name}.wasm (shared across every project that uses it)...");
        let module = agentplug_host::load_module_file_backed(
            &self.engine,
            &wasm_path,
            plugin_name,
            &on_disk_hash,
        )?;
        self.install_compiled(plugin_name, module, on_disk_hash, &wasm_path);
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

mod claim_dispatch;
pub(crate) use claim_dispatch::*;

fn github_cli_config_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("AGENTPLUG_GH_CONFIG_DIR") {
        candidates.push((PathBuf::from(path), "AGENTPLUG_GH_CONFIG_DIR"));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push((PathBuf::from(path).join("gh"), "XDG_CONFIG_HOME"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push((
            PathBuf::from(&home).join(".gmweb/cache/.config/gh"),
            "GM web credential cache",
        ));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push((
            PathBuf::from(home).join("Library/Application Support/gh"),
            "HOME",
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push((PathBuf::from(home).join(".config/gh"), "HOME"));
    }
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("APPDATA") {
        candidates.push((PathBuf::from(path).join("GitHub CLI"), "APPDATA"));
    }
    candidates
}

fn usable_github_cli_config_dir(requested: &Path) -> Option<PathBuf> {
    if !requested.is_absolute() {
        return None;
    }
    let directory = requested.canonicalize().ok()?;
    if !directory.is_dir() || !directory.join("hosts.yml").is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if directory.metadata().ok()?.uid() != unsafe { libc::geteuid() } {
            return None;
        }
    }
    Some(directory)
}

fn configure_github_cli_config_dir() -> bool {
    if std::env::var_os("GH_CONFIG_DIR").is_some() {
        agentplug_host::set_github_cli_config_dir(None);
        return true;
    }
    let mut seen = HashSet::new();
    for (candidate, source) in github_cli_config_candidates() {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        let Some(directory) = usable_github_cli_config_dir(&candidate) else {
            continue;
        };
        agentplug_host::set_github_cli_config_dir(Some(directory));
        eprintln!(
            "[agentplug daemon] configured GitHub CLI credentials from {source} without copying credential data"
        );
        return true;
    }
    false
}

pub fn run_daemon() -> anyhow::Result<()> {
    if let Some(owner_pid) = shared_daemon_owner_that_would_refuse_this_process() {
        record_wasted_daemon_start();
        eprintln!(
            "[agentplug daemon] shared daemon pid {owner_pid} already owns the ownership lock and its heartbeat is fresh -- exiting before the registry announce and credential-store discovery, nothing shared was touched"
        );
        return Ok(());
    }

    eprintln!(
        "[agentplug daemon] starting, registry {}",
        registry_path().display()
    );

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
    runner_version_parity();

    let plugin_modules = PluginModules::new()?;
    let previously_recorded_version = installed_runner_version();
    if previously_recorded_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        crate::download::clear_all_known_bad_version_markers();
        let _ = record_runner_version(env!("CARGO_PKG_VERSION"));
    }
    crate::download::sync_local_build_pin();
    run_daemon_body(plugin_modules)
}

fn spawn_update_poll_worker(
    daemon_cfg: DaemonConfig,
) -> std::sync::mpsc::Receiver<(PathBuf, String)> {
    let (staged_tx, staged_rx) = std::sync::mpsc::channel::<(PathBuf, String)>();
    let spawned = std::thread::Builder::new().name("update-poll".to_string()).spawn(move || {
        let plugin_update_poll_interval = daemon_cfg.plugin_update_poll_interval();
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
        let mut first_runner_poll_pending = true;
        let persisted_runner_poll_ts_at_boot = read_persisted_poll_ts(&persisted_runner_poll_ts_path());
        if persisted_runner_poll_ts_at_boot > 0 {
            HEARTBEAT_LAST_RUNNER_POLL_TS.store(persisted_runner_poll_ts_at_boot, std::sync::atomic::Ordering::Relaxed);
        }
        loop {
            let forced_refresh_request = take_forced_plugin_refresh_request();
            if last_plugin_update_poll.elapsed() >= shortest_plugin_poll_interval || forced_refresh_request.is_some() {
                last_plugin_update_poll = Instant::now();
                let poll_ts = now_ms();
                HEARTBEAT_LAST_PLUGIN_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
                write_persisted_poll_ts(&persisted_plugin_poll_ts_path(), poll_ts);
                let targets: Vec<String> = match &forced_refresh_request {
                    Some(Some(name)) => vec![name.clone()],
                    _ => loaded_plugin_versions().lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect(),
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
                                "[agentplug daemon] downloaded+verified plugin {plugin_name} update to {new_version} -- the next tick's get_or_compile content-hash check recompiles it off the claim loop and swaps it in when ready"
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

            if first_runner_poll_pending || last_runner_update_poll.elapsed() >= runner_update_poll_interval || take_forced_runner_refresh_request() {
                first_runner_poll_pending = false;
                last_runner_update_poll = Instant::now();
                let poll_ts = now_ms();
                HEARTBEAT_LAST_RUNNER_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
                write_persisted_poll_ts(&persisted_runner_poll_ts_path(), poll_ts);
                match crate::download::stage_runner_self_update() {
                    Ok(Some(staged_and_version)) => {
                        if staged_tx.send(staged_and_version).is_err() {
                            return;
                        }
                    }
                    Ok(None) => record_runner_poll_error(None),
                    Err(e) => {
                        let msg = format!("runner self-update check failed: {e}");
                        eprintln!("[agentplug daemon] {msg}");
                        record_runner_poll_error(Some(msg));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    if let Err(e) = spawned {
        eprintln!("[agentplug daemon] could not start the update-poll thread: {e} -- plugin and runner updates will not be polled until the daemon restarts");
    }
    staged_rx
}

fn run_daemon_body(mut plugin_modules: PluginModules) -> anyhow::Result<()> {
    configure_github_cli_config_dir();
    HEARTBEAT_DAEMON_BOOT_TS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    write_daemon_heartbeat(0, 0);
    let parity = runner_version_parity();
    eprintln!(
        "[agentplug daemon] BOOT pid={} version={} exe_sha256={} runner_version_parity={} ts={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        parity.live_sha256(),
        if parity.disagreements().is_empty() {
            "ok"
        } else {
            "MISMATCH"
        },
        now_ms()
    );
    for disagreement in parity.disagreements() {
        eprintln!("[agentplug daemon] RUNNER VERSION PARITY MISMATCH: {disagreement}");
    }

    ensure_daemon_guard();

    let daemon_cfg = DaemonConfig::load();
    let registry_poll_interval = daemon_cfg.registry_poll_interval();
    let heartbeat_interval = daemon_cfg.heartbeat_interval();
    GM_PROCESSOR_CAPACITY.store(
        daemon_cfg.gm_pool_size(),
        std::sync::atomic::Ordering::Relaxed,
    );
    SHARED_STORE_RECYCLE_LIMIT_MB.store(
        daemon_cfg.shared_store_recycle_private_bytes() / (1024 * 1024),
        std::sync::atomic::Ordering::Relaxed,
    );
    *gm_processor_capacity_reason()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = daemon_cfg.gm_pool_capacity_reason();
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
    let mut roots_new_this_registry_poll: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    let mut last_cold_project_sweep = Instant::now()
        .checked_sub(COLD_PROJECT_SWEEP_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut cold_sweep_pending = false;
    let mut cold_sweep_walk = RegistryWalk { cursor: 0 };
    let mut project_round_robin_cursor = 0usize;
    let mut background_round_robin_cursor = 0usize;
    let mut last_per_root_plugin_scan = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    let mut per_root_plugin_scan_pending = false;
    let mut per_root_plugin_scan_walk = RegistryWalk { cursor: 0 };

    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    const IDLE_WAIT_MIN_MS: u64 = 25;
    const IDLE_WAIT_MAX_MS: u64 = 1_000;
    const PENDING_UNCLAIMED_IDLE_WAIT_MAX_MS: u64 = 250;
    let mut idle_wait_ms = IDLE_WAIT_MIN_MS;

    let shared_plugin_release_idle_ms = daemon_cfg.shared_plugin_release_idle_ms();
    let mut last_shared_release = Instant::now();

    let instruction_source_poll_interval = daemon_cfg.instruction_source_poll_interval();
    let staged_runner_rx = spawn_update_poll_worker(daemon_cfg.clone());
    #[cfg(windows)]
    let mut idle_in_dir_watch = IdleInDirWatch::new();
    let mut pending_self_update: Option<(PathBuf, String)> = None;
    let mut pending_self_update_staged_at: Option<Instant> = None;
    const SELF_UPDATE_MAX_STARVED_MS: u64 = 10 * 60 * 1000;

    if let Some((staged_at_ms, _len)) = staged_runner_awaiting_handoff() {
        if let Some(staged_path) = canonical_runner_exe_path().map(|c| {
            c.with_extension(
                c.extension()
                    .map(|e| format!("{}.new", e.to_string_lossy()))
                    .unwrap_or_else(|| "new".to_string()),
            )
        }) {
            let staged_age = now_ms().saturating_sub(staged_at_ms);
            let mut boot_check_cmd = std::process::Command::new(&staged_path);
            boot_check_cmd.arg("--version");
            agentplug_host::apply_windowless(&mut boot_check_cmd);
            match boot_check_cmd.output() {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout)
                        .split_whitespace()
                        .last()
                        .unwrap_or_default()
                        .trim_start_matches('v')
                        .to_string();
                    if version.is_empty() {
                        eprintln!(
                            "[agentplug daemon] pre-existing staged runner {} printed an unparseable --version ({:?}) -- ignoring it rather than handing off to an unknown version",
                            staged_path.display(), String::from_utf8_lossy(&out.stdout).trim()
                        );
                    } else {
                        eprintln!(
                            "[agentplug daemon] found pre-existing staged runner {} (version {version}) at boot, age {}ms -- adopting its on-disk mtime so a daemon restart does not reset the starve clock",
                            staged_path.display(), staged_age
                        );
                        pending_self_update = Some((staged_path.clone(), version));
                        pending_self_update_staged_at =
                            Instant::now().checked_sub(Duration::from_millis(staged_age));
                    }
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
    let (instruction_source_sync_done_tx, instruction_source_sync_done_rx) =
        std::sync::mpsc::channel::<PathBuf>();

    let _heartbeat_ticker = spawn_heartbeat_ticker(heartbeat_interval);
    write_daemon_heartbeat(0, 0);

    const PROJECT_HEARTBEAT_TICK_INTERVAL_MS: u64 = 3_000;
    let _project_heartbeat_ticker =
        spawn_project_heartbeat_ticker(Duration::from_millis(PROJECT_HEARTBEAT_TICK_INTERVAL_MS));

    const DREAM_RSI_CYCLE_TICK_INTERVAL_MS: u64 = 60_000;
    let _dream_rsi_cycle_ticker =
        spawn_dream_rsi_cycle_ticker(Duration::from_millis(DREAM_RSI_CYCLE_TICK_INTERVAL_MS));

    loop {
        if heartbeat_authority_lost() {
            let requeued = hand_claims_to_live_successor("heartbeat-authority-holder");
            sweep_orphaned_claims_across_roots(&known_roots);
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- re-queued {requeued} in-flight claim(s) for it and exiting before serving further work");
            return Ok(());
        }

        if first_registry_poll_pending || last_registry_poll.elapsed() >= registry_poll_interval {
            let sweep_orphans_left_by_whatever_daemon_died_before_answering =
                first_registry_poll_pending;
            first_registry_poll_pending = false;
            last_registry_poll = Instant::now();
            let previous_roots: std::collections::HashSet<PathBuf> =
                known_roots.iter().cloned().collect();
            known_roots = read_registry();
            roots_new_this_registry_poll = known_roots
                .iter()
                .filter(|r| !previous_roots.contains(*r))
                .cloned()
                .collect();
            for root in &roots_new_this_registry_poll {
                mark_spool_dirty(root);
            }
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

        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

        const PER_ROOT_PLUGIN_SCAN_INTERVAL: Duration = Duration::from_secs(5);
        const PER_ROOT_PASS_BUDGET: Duration = Duration::from_millis(250);
        if per_root_plugin_scan_pending {
            let completed = per_root_plugin_scan_walk.walk(
                &known_roots,
                PER_ROOT_PASS_BUDGET,
                "per-root-plugin-scan",
                |root| {
                    refresh_dispatch_wait_ledger(root);
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
                },
            );
            if completed {
                per_root_plugin_scan_pending = false;
                last_per_root_plugin_scan = Instant::now();
            }
        }
        if !per_root_plugin_scan_pending
            && (!roots_new_this_registry_poll.is_empty()
                || last_per_root_plugin_scan.elapsed() >= PER_ROOT_PLUGIN_SCAN_INTERVAL)
        {
            per_root_plugin_scan_pending = true;
            per_root_plugin_scan_walk.reset();
        }
        for plugin_name in ["gm", "libsql", "bert", "treesitter", "crux", "lightpanda"] {
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
        agentplug_host::set_sibling_reload_source((
            plugin_modules.engine.clone(),
            plugin_modules.modules_with_hashes(),
        ));

        const COLD_SWEEP_PASS_BUDGET: Duration = Duration::from_millis(500);
        let mut cold_sweep_ran_this_tick = false;
        if cold_sweep_pending {
            cold_sweep_ran_this_tick = true;
            let no_inherited_claims = HashSet::new();
            let completed = cold_sweep_walk.walk(
                &known_roots,
                COLD_SWEEP_PASS_BUDGET,
                "cold-sweep",
                |root| {
                    sweep_orphaned_claims_distinguishing_handoff_from_crash(
                        root,
                        &no_inherited_claims,
                    );
                    reap_spool_out_files(root, false);
                    sweep_unconsumable_spool_files(root);
                },
            );
            if completed {
                cold_sweep_pending = false;
                last_cold_project_sweep = Instant::now();
            }
        }
        if !cold_sweep_pending
            && last_cold_project_sweep.elapsed() >= COLD_PROJECT_SWEEP_INTERVAL
        {
            cold_sweep_pending = true;
            cold_sweep_walk.reset();
        }
        let mut all_projects: Vec<(PathBuf, ProjectPlugins)> =
            Vec::with_capacity(known_roots.len());
        let mut is_genuinely_active: Vec<bool> = Vec::with_capacity(known_roots.len());
        let mut skipped_cold = 0usize;
        for root in &known_roots {
            match projects.remove(root) {
                Some(p) => {
                    all_projects.push((root.clone(), p));
                    is_genuinely_active.push(project_has_queued_spool_work_cached(root));
                }
                None if cold_sweep_ran_this_tick
                    || roots_new_this_registry_poll.contains(root)
                    || project_has_queued_spool_work_cached(root) =>
                {
                    all_projects.push((root.clone(), ProjectPlugins::new(root.clone())));
                    is_genuinely_active.push(
                        roots_new_this_registry_poll.contains(root)
                            || project_has_queued_spool_work_cached(root),
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
                    write_project_heartbeat_rate_limited(
                        root,
                        read_status_busy_until_if_future(root),
                        Some((position, reported_queue_total)),
                    );
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
        if !background_projects.is_empty() {
            let len = background_projects.len();
            background_projects.rotate_left(background_round_robin_cursor % len);
            background_round_robin_cursor = (background_round_robin_cursor + worker_count) % len;
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
                        let Some((root, mut project)) = next else {
                            break;
                        };
                        let did_work =
                            dispatch_project(root.as_path(), &mut project, plugin_modules_ref);
                        done_ref
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((root, project, did_work));
                    }));
                }
                for h in handles {
                    let _ = h.join();
                }
            });
        }
        let mut any_work = false;
        for (root, project, did_work) in done.into_inner().unwrap_or_else(|e| e.into_inner()) {
            any_work = any_work || did_work;
            if reported_queue_total > worker_count {
                let spool_dir = root.join(".gm").join("exec-spool");
                if fs::create_dir_all(&spool_dir).is_ok() {
                    write_project_heartbeat_rate_limited(
                        root.as_path(),
                        read_status_busy_until_if_future(root.as_path()),
                        Some((0, 0)),
                    );
                }
            }
            projects.insert(root, project);
        }
        HEARTBEAT_PROJECT_COUNT.store(projects.len(), std::sync::atomic::Ordering::Relaxed);
        HEARTBEAT_PLUGIN_MODULE_COUNT.store(
            plugin_modules.modules.len(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if heartbeat_authority_lost() {
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            mark_intentional_exit("heartbeat-authority-lost");
            return Ok(());
        }
        if let Some((version, requeued)) = hand_off_to_ready_successor() {
            eprintln!(
                "[agentplug daemon] ready takeover successor {version} reserved ownership after a safe batch boundary -- re-queued {requeued} inherited claim(s) and exiting"
            );
            return Ok(());
        }

        let evict_before = Instant::now()
            .checked_sub(Duration::from_millis(daemon_cfg.project_idle_evict_ms()))
            .unwrap_or_else(Instant::now);
        let to_evict: Vec<PathBuf> = projects
            .iter()
            .filter(|(_, p)| p.last_active < evict_before)
            .map(|(root, _)| root.clone())
            .collect();
        for root in to_evict {
            eprintln!(
                "[agentplug daemon] evicting idle project {}",
                root.display()
            );
            projects.remove(&root);
        }

        while let Ok((staged, version)) = staged_runner_rx.try_recv() {
            if handoff_backed_off(&version) {
                let _ = fs::remove_file(&staged);
                eprintln!(
                    "[agentplug daemon] staged self-update to {version} is inside its {}s retry backoff after a failed handoff -- dropping it instead of retrying on every tick",
                    HANDOFF_RETRY_BACKOFF.as_secs()
                );
            } else {
                eprintln!(
                    "[agentplug daemon] staged self-update to {version} at {}",
                    staged.display()
                );
                if pending_self_update.is_none() {
                    pending_self_update_staged_at = Some(Instant::now());
                }
                pending_self_update = Some((staged, version));
                record_runner_poll_error(None);
            }
        }

        let self_update_starved = pending_self_update_staged_at
            .map(|staged_at| {
                staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_MAX_STARVED_MS)
            })
            .unwrap_or(false);
        let detached_still_running = !in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        const SELF_UPDATE_HARD_CAP_MS: u64 = SELF_UPDATE_MAX_STARVED_MS + 60_000;
        let self_update_hard_capped = pending_self_update_staged_at
            .map(|staged_at| staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_HARD_CAP_MS))
            .unwrap_or(false);
        let force_handoff_despite_in_flight =
            self_update_starved && detached_still_running && self_update_hard_capped;
        let force_handoff_never_idle =
            self_update_starved && self_update_hard_capped && !detached_still_running && any_work;
        let self_update_backed_off = pending_self_update
            .as_ref()
            .map(|(_, v)| handoff_backed_off(v))
            .unwrap_or(false);
        if self_update_backed_off {
            pending_self_update_staged_at = None;
        }
        let task_handoff = if pending_self_update.is_some()
            && !self_update_backed_off
            && ((!any_work && !detached_still_running)
                || force_handoff_despite_in_flight
                || force_handoff_never_idle)
        {
            match agentplug_host::prepare_task_handoff() {
                Ok(guard) => guard,
                Err(error) => {
                    eprintln!("[agentplug daemon] task result preservation refuses staged runner handoff: {error}");
                    None
                }
            }
        } else {
            None
        };
        if let Some(task_handoff) = task_handoff {
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
                    task_handoff.commit();
                    let requeued =
                        requeue_claims_for_live_successor(&claims_the_successor_inherits);
                    eprintln!(
                        "[agentplug daemon] handed off to version {version} -- re-queued {requeued} of {} inherited claim(s) for the incoming daemon, exiting",
                        claims_the_successor_inherits.len()
                    );
                    mark_intentional_exit("runner-handoff");
                    return Ok(());
                }
                clear_handoff_inherited_claims();
                if staged.exists() {
                    pending_self_update = Some((staged, version));
                } else {
                    pending_self_update_staged_at = None;
                    eprintln!(
                        "[agentplug daemon] dropping the self-update to {version}: the staged exe {} no longer exists after the failed handoff, so retrying it every tick would only fail faster -- the next scheduled poll re-stages it",
                        staged.display()
                    );
                }
            }
        }

        if let Some(reason) =
            shared_store_recycle_reason_independent_of_daemon_idle_state(&daemon_cfg)
        {
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
        } else if last_shared_release.elapsed()
            >= Duration::from_millis(shared_plugin_release_idle_ms)
        {
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
        } else if last_any_dispatch.elapsed() >= Duration::from_millis(SELF_RECYCLE_IDLE_MS)
            && !detached_still_running
        {
            eprintln!(
                "[agentplug daemon] self-recycling after {}ms fully idle -- reclaims shared-plugin peak wasm memory (monotonic linear memory, no in-place shrink); next real dispatch spawns a fresh process",
                SELF_RECYCLE_IDLE_MS
            );
            mark_intentional_exit("self-recycle");
            return Ok(());
        }

        let pending_work_exists_before_idle_wait = known_roots
            .iter()
            .any(|root| project_has_pending_dispatch_work(root));
        // Work that was claimed resets the wait to the minimum. Queued files that
        // stay unclaimable (unsettled, unreadable, or stale) must not pin the loop
        // at the minimum, so their wait backs off to a short cap instead.
        if any_work {
            idle_wait_ms = IDLE_WAIT_MIN_MS;
        }
        #[cfg(windows)]
        wait_for_in_dir_change(
            &mut idle_in_dir_watch,
            &known_roots,
            Duration::from_millis(idle_wait_ms),
        );
        #[cfg(not(windows))]
        std::thread::sleep(Duration::from_millis(idle_wait_ms));
        if !any_work {
            let cap_ms = if pending_work_exists_before_idle_wait {
                PENDING_UNCLAIMED_IDLE_WAIT_MAX_MS
            } else {
                IDLE_WAIT_MAX_MS
            };
            idle_wait_ms = (idle_wait_ms.saturating_mul(2)).min(cap_ms);
        }
    }
}
