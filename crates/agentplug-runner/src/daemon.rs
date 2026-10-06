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
    build_engine, install_dir, now_ms, read_project_plugin_list, DispatchHandle, GmFairnessGuard,
    ProjectPlugins, ToolDispatchGuard,
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
    "serp",
    "browser",
    "cdp",
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
    "git_poll",
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
    #[serde(default)]
    require_runner_signature: Option<bool>,
}

const DAEMON_CONFIG_EXAMPLE: &str = r#"{
  "registry_poll_interval_secs": 5,
  "heartbeat_interval_secs": 10,
  "plugin_update_poll_interval_secs": 600,
  "plugin_update_poll_interval_secs_by_name": {},
  "runner_update_poll_interval_secs": 60,
  "instruction_source_poll_interval_secs": 600,
  "require_runner_signature": false
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
            require_runner_signature: None,
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
    fn require_runner_signature(&self) -> bool {
        self.require_runner_signature.unwrap_or(false)
    }
}

pub(crate) fn daemon_requires_runner_signature() -> bool {
    DaemonConfig::load().require_runner_signature()
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

pub fn arm_spool_launcher_deadline() {
    std::thread::spawn(|| {
        std::thread::sleep(SPOOL_LAUNCHER_HARD_DEADLINE);
        eprintln!("[agentplug] spool launcher exceeded {}s without converging -- exiting so a wedged launch never lingers", SPOOL_LAUNCHER_HARD_DEADLINE.as_secs());
        std::process::exit(2);
    });
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

fn staged_binary_self_check(staged_exe: &Path, expected_version: &str) -> bool {
    let mut cmd = std::process::Command::new(staged_exe);
    cmd.arg("--version");
    agentplug_host::apply_windowless(&mut cmd);
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
    if let Err(problem) = crate::update_trust::staged_runner_permitted(staged_exe) {
        let _ = fs::remove_file(staged_exe);
        crate::update_trust::remove_stage_record(staged_exe);
        eprintln!(
            "[agentplug daemon] refusing to hand off to {version}: {problem} -- staged exe removed and the running version kept"
        );
        record_handoff_failure(
            version,
            format!("update-signature verification refused staged {version}: {problem}"),
        );
        return false;
    }
    if !staged_binary_self_check(staged_exe, version) {
        let _ = fs::remove_file(staged_exe);
        crate::update_trust::remove_stage_record(staged_exe);
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

fn promote_staged_exe_to_canonical(version: &str, running_before: Option<&str>) -> bool {
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
    if let Err(problem) = crate::update_trust::staged_runner_permitted(&staged) {
        eprintln!(
            "[agentplug daemon] takeover: refusing to promote {version} onto {} -- {problem}; this process keeps running from the staged copy instead of overwriting the canonical exe",
            canonical.display()
        );
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
            crate::update_trust::record_unverified_promotion(&staged, version, running_before);
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
    for plugin_name in ["gm", "bert", "libsql", "treesitter", "oxibrowser", "crux"] {
        if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
            eprintln!("[agentplug daemon] takeover: pre-warm of {plugin_name} failed (non-fatal, will lazy-compile on first use): {e}");
        }
    }
    let _ = fs::write(
        takeover_ready_path(),
        serde_json::json!({"version": version, "pid": std::process::id(), "ts": now_ms()})
            .to_string(),
    );
    let running_before = crate::download::installed_runner_version();
    eprintln!("[agentplug daemon] takeover: readiness marker written, waiting for old daemon to release ownership");
    for _ in 0..480 {
        if read_owner_pid().is_none() && claim_ownership() {
            if let Err(error) = record_runner_version(version) {
                eprintln!(
                    "[agentplug daemon] takeover: could not record running version {version}: {error} -- continuing with the verified staged runner so the old daemon is not left without a successor; a later boot will reconcile the marker"
                );
            } else {
                crate::download::clear_all_known_bad_version_markers();
            }
            let promoted = promote_staged_exe_to_canonical(version, running_before.as_deref());
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
            "runner_update_trust_mode": crate::update_trust::runner_mode_str(),
            "runner_signature_required": crate::update_trust::strict_mode(),
            "runner_unverified_update": crate::update_trust::unverified_promotion().unwrap_or(serde_json::Value::Null),
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

fn write_project_heartbeat(spool_dir: &Path, busy_until: Option<u64>) {
    write_project_heartbeat_with_queue_info(spool_dir, busy_until, None);
}

fn write_project_heartbeat_with_queue_info(
    spool_dir: &Path,
    busy_until: Option<u64>,
    queue_info: Option<(usize, usize)>,
) {
    let status_path = spool_dir.join(".status.json");
    let mut payload = match fs::read_to_string(&status_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
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
    payload["gm_processor_capacity"] =
        serde_json::json!(GM_PROCESSOR_CAPACITY.load(std::sync::atomic::Ordering::Relaxed));
    payload["gm_processor_capacity_reason"] = serde_json::json!(gm_processor_capacity_reason()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone());
    payload["shared_store_recycle_limit_mb"] =
        serde_json::json!(SHARED_STORE_RECYCLE_LIMIT_MB.load(std::sync::atomic::Ordering::Relaxed));
    payload["tool_serialization"] = serde_json::json!("fifo per plugin and verb for state-changing verbs, one dispatch per project lane (git, store, state); exec-family, browser, read-only verbs and tree-scan codesearch run unserialised");
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
    let _ = fs::write(&status_path, payload.to_string());
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
        for root in roots {
            let spool_dir = root.join(".gm").join("exec-spool");
            if !spool_dir.exists() {
                continue;
            }
            write_project_heartbeat(&spool_dir, busy_until_for_project_ticker(&root, &spool_dir));
        }
    })
}

static DREAM_RSI_LAST_CYCLE_DISPATCH_TS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();

fn dream_rsi_last_cycle_dispatch_ts() -> &'static Mutex<HashMap<PathBuf, u64>> {
    DREAM_RSI_LAST_CYCLE_DISPATCH_TS.get_or_init(|| Mutex::new(HashMap::new()))
}

const DREAM_RSI_MIN_NEW_OBSERVATIONS_PER_CYCLE: u64 = 20;
const DREAM_RSI_MIN_CYCLE_SPACING_MS: u64 = 15 * 60 * 1000;

fn dream_rsi_count_observations(dream_rsi_dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dream_rsi_dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let obs_path = entry.path().join("observations.json");
        let Ok(bytes) = fs::read(&obs_path) else {
            continue;
        };
        if let Ok(serde_json::Value::Array(arr)) =
            serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            total += arr.len() as u64;
        }
    }
    total
}

fn dream_rsi_maybe_dispatch_cycle(root: &Path, spool_dir: &Path) {
    let dream_rsi_dir = root.join(".gm").join("dream-rsi");
    if !dream_rsi_dir.exists() {
        return;
    }
    let observation_count = dream_rsi_count_observations(&dream_rsi_dir);
    let now = now_ms();

    let mut last_ts_map = dream_rsi_last_cycle_dispatch_ts()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let last_fired = last_ts_map.get(root).copied();
    let cursor_path = dream_rsi_dir.join(".last-cycle-observation-count");
    let baseline_count: u64 = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let spacing_ok = last_fired
        .map(|ts| now.saturating_sub(ts) >= DREAM_RSI_MIN_CYCLE_SPACING_MS)
        .unwrap_or(true);
    let new_observations = observation_count.saturating_sub(baseline_count);
    let data_threshold_met = new_observations >= DREAM_RSI_MIN_NEW_OBSERVATIONS_PER_CYCLE;

    if !(spacing_ok && data_threshold_met) {
        return;
    }

    let in_dir = spool_dir.join("in").join("dreamrsi-replay");
    if fs::create_dir_all(&in_dir).is_err() {
        return;
    }
    let session_id = format!("daemon-dreamrsi-tick-{now}");
    let payload = serde_json::json!({
        "SESSION_ID": session_id,
        "trigger": "unattended-daemon-tick",
        "new_observation_count": new_observations,
        "total_observation_count": observation_count,
        "authorization_note": "read-only replay/scoring; no unattended redeploy -- see .gm/next-step.md Grounded Dream-RSI replay",
    })
    .to_string();

    let tmp_path = in_dir.join(format!(".tmp-{now}"));
    let final_path = in_dir.join(format!("{session_id}-1.txt"));
    if fs::write(&tmp_path, &payload).is_err() {
        return;
    }
    if fs::rename(&tmp_path, &final_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return;
    }

    last_ts_map.insert(root.to_path_buf(), now);
    drop(last_ts_map);
    let _ = fs::write(&cursor_path, observation_count.to_string());
}

fn spawn_dream_rsi_cycle_ticker(interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        if heartbeat_authority_lost() {
            return;
        }
        let roots = known_project_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for root in roots {
            let spool_dir = root.join(".gm").join("exec-spool");
            if !spool_dir.exists() {
                continue;
            }
            dream_rsi_maybe_dispatch_cycle(&root, &spool_dir);
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

fn busy_until_for_project_ticker(root: &Path, spool_dir: &Path) -> Option<u64> {
    if project_in_flight_count(root) > 0 || spool_has_queued_work(spool_dir) {
        return Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS);
    }
    read_status_busy_until_if_future(spool_dir)
}

fn spool_has_queued_work(spool_dir: &Path) -> bool {
    let in_dir = spool_dir.join("in");
    let Ok(verbs) = fs::read_dir(&in_dir) else {
        return false;
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
            let name = file_entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".inflight") || is_spool_request_path(&verb, &file_entry.path()) {
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
    let Ok(verbs) = fs::read_dir(in_dir) else {
        return (queued, claimed);
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
            let name = file_entry.file_name();
            let name = name.to_string_lossy();
            if is_spool_request_path(&verb, &file_entry.path()) {
                queued += 1;
            } else if name.ends_with(".inflight") {
                claimed += 1;
            }
        }
    }
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

pub(crate) type InFlightKey = (PathBuf, String, String);

pub(crate) struct InFlightHandle {
    pub(crate) detach: Arc<std::sync::atomic::AtomicBool>,
}

static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

pub(crate) fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

const MAX_CLAIMED_DISPATCHES_PER_PROJECT: usize = 32;

fn project_in_flight_count(root: &Path) -> usize {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|(active_root, _, _)| active_root == root)
        .count()
}

struct InFlightEntryRelease {
    key: InFlightKey,
}

impl Drop for InFlightEntryRelease {
    fn drop(&mut self) {
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
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
            handle
                .detach
                .store(true, std::sync::atomic::Ordering::SeqCst);
            serde_json::json!({"ok": true, "converted": true, "verb": req.verb, "task": req.task})
                .to_string()
        }
        None => {
            let out_path = root
                .join(".gm")
                .join("exec-spool")
                .join("out")
                .join(format!("{}-{}.json", req.verb, req.task));
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
    let requested_plugin = parsed
        .as_ref()
        .and_then(|v| v.get("plugin").and_then(|p| p.as_str()).map(str::to_string));
    let also_runner = parsed
        .as_ref()
        .and_then(|v| v.get("runner").and_then(|r| r.as_bool()))
        .unwrap_or(false);

    let marker = force_plugin_refresh_marker_path();
    let contents = requested_plugin.as_deref().unwrap_or("").to_string();
    let _ = fs::write(&marker, contents);

    if also_runner {
        let _ = fs::write(force_runner_refresh_marker_path(), b"");
    }

    let local_dev_sideload = requested_plugin
        .as_deref()
        .and_then(crate::download::read_local_dev_sideload_marker);

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
    Some(if contents.trim().is_empty() {
        None
    } else {
        Some(contents.trim().to_string())
    })
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

fn spool_in_file_write_has_settled(request_path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(request_path) else {
        return false;
    };
    metadata.len() > 0
}

fn language_spool_extension(verb: &str) -> Option<&'static str> {
    match verb {
        "nodejs" => Some("js"),
        "python" => Some("py"),
        "bash" => Some("sh"),
        "powershell" => Some("ps1"),
        "typescript" | "deno" => Some("ts"),
        "go" => Some("go"),
        "rust" => Some("rs"),
        "c" => Some("c"),
        "cpp" => Some("cpp"),
        "java" => Some("java"),
        _ => None,
    }
}

const UNIVERSAL_SPOOL_REQUEST_EXTENSION: &str = "txt";

fn spool_request_extension(verb: &str) -> &'static str {
    language_spool_extension(verb).unwrap_or(UNIVERSAL_SPOOL_REQUEST_EXTENSION)
}

fn accepted_spool_request_extensions(verb: &str) -> [&'static str; 2] {
    [
        spool_request_extension(verb),
        UNIVERSAL_SPOOL_REQUEST_EXTENSION,
    ]
}

fn is_spool_request_path(verb: &str, request_path: &Path) -> bool {
    let Some(extension) = request_path
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return false;
    };
    accepted_spool_request_extensions(verb).contains(&extension)
}

fn spool_claim_path(request_path: &Path) -> Option<PathBuf> {
    let extension = request_path.extension()?.to_str()?;
    Some(request_path.with_extension(format!("{extension}.{ORPHAN_CLAIM_EXT}")))
}

pub fn claim_spool_request_in_place(request_path: &Path) -> Option<PathBuf> {
    if !spool_in_file_write_has_settled(request_path) {
        return None;
    }
    let claim_path = spool_claim_path(request_path)?;
    fs::rename(request_path, &claim_path)
        .ok()
        .map(|_| claim_path)
}

const EXEC_OUTPUT_SPILL_THRESHOLD_CHARS: usize = 2000;

fn exec_output_field_text(envelope: &serde_json::Value, field: &str) -> Option<String> {
    match envelope.get(field)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) if text.is_empty() => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

fn spill_large_exec_output_to_text_sibling(
    out_dir: &Path,
    verb: &str,
    task: &str,
    out_body: String,
) -> String {
    let Ok(mut outer) = serde_json::from_str::<serde_json::Value>(&out_body) else {
        return out_body;
    };
    let Some(envelope_text) = outer.get("data").and_then(|d| d.as_str()) else {
        return out_body;
    };
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(envelope_text) else {
        return out_body;
    };
    if !envelope.get("stdout").is_some_and(|v| v.is_string()) {
        return out_body;
    }
    let sections: Vec<(&str, String)> = ["result", "stdout", "stderr"]
        .into_iter()
        .filter_map(|field| exec_output_field_text(&envelope, field).map(|text| (field, text)))
        .collect();
    if !sections
        .iter()
        .any(|(_, text)| text.chars().count() > EXEC_OUTPUT_SPILL_THRESHOLD_CHARS)
    {
        return out_body;
    }
    let mut rendered = String::new();
    for (field, text) in &sections {
        rendered.push_str(&format!("## {field}\n{text}\n\n"));
    }
    let sibling_name = format!("{verb}-{task}.txt");
    let sibling = out_dir.join(&sibling_name);
    if fs::write(&sibling, rendered).is_err() {
        return out_body;
    }
    let Some(obj) = outer.as_object_mut() else {
        return out_body;
    };
    obj.insert(
        "result_file".to_string(),
        serde_json::Value::String(sibling.to_string_lossy().into_owned()),
    );
    outer.to_string()
}

pub fn write_spool_out_confirmed(out_dir: &Path, out_name: &str, out_body: &str) -> bool {
    let dest = out_dir.join(out_name);
    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
    if fs::write(&tmp, out_body).is_ok() && fs::rename(&tmp, &dest).is_ok() {
        let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
        return dest.exists();
    }
    let _ = fs::remove_file(&tmp);
    dest.exists()
}

fn forget_in_flight_claim(in_dir: &Path, verb: &str, task: &str) {
    let Some(root) = in_dir.ancestors().nth(3) else {
        return;
    };
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(root.to_path_buf(), verb.to_string(), task.to_string()));
}

fn write_spool_out_and_release_claim(
    out_dir: &Path,
    in_dir: &Path,
    verb: &str,
    task: &str,
    out_body: &str,
) {
    forget_in_flight_claim(in_dir, verb, task);
    if write_spool_out_confirmed(out_dir, &format!("{verb}-{task}.json"), out_body) {
        let _ = fs::remove_file(inflight_claim_path(in_dir, verb, task));
    } else {
        eprintln!("[agentplug daemon] out-file write for {verb}/{task} did not confirm -- leaving the claim for the orphan sweep instead of deleting an unanswered request");
    }
}

const ORPHAN_CLAIM_EXT: &str = "inflight";

fn inflight_claim_path_with_extension(
    in_dir: &Path,
    verb: &str,
    task: &str,
    extension: &str,
) -> PathBuf {
    in_dir
        .join(verb)
        .join(format!("{task}.{extension}.{ORPHAN_CLAIM_EXT}"))
}

fn existing_inflight_claim(
    in_dir: &Path,
    verb: &str,
    task: &str,
) -> Option<(PathBuf, &'static str)> {
    accepted_spool_request_extensions(verb)
        .into_iter()
        .map(|extension| {
            (
                inflight_claim_path_with_extension(in_dir, verb, task, extension),
                extension,
            )
        })
        .find(|(claim, _)| claim.exists())
}

fn inflight_claim_path(in_dir: &Path, verb: &str, task: &str) -> PathBuf {
    existing_inflight_claim(in_dir, verb, task)
        .map(|(claim, _)| claim)
        .unwrap_or_else(|| {
            inflight_claim_path_with_extension(in_dir, verb, task, spool_request_extension(verb))
        })
}

fn queued_request_path_with_extension(
    in_dir: &Path,
    verb: &str,
    task: &str,
    extension: &str,
) -> PathBuf {
    in_dir.join(verb).join(format!("{task}.{extension}"))
}

fn any_queued_request_exists(in_dir: &Path, verb: &str, task: &str) -> bool {
    accepted_spool_request_extensions(verb)
        .into_iter()
        .any(|extension| queued_request_path_with_extension(in_dir, verb, task, extension).exists())
}

fn project_in_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("exec-spool").join("in")
}

type AbandonedClaim = (PathBuf, String, String);

fn requeue_claim(in_dir: &Path, verb: &str, task: &str) -> bool {
    let Some((claim, extension)) = existing_inflight_claim(in_dir, verb, task) else {
        return false;
    };
    if any_queued_request_exists(in_dir, verb, task) {
        let _ = fs::remove_file(&claim);
        return true;
    }
    fs::rename(
        &claim,
        queued_request_path_with_extension(in_dir, verb, task, extension),
    )
    .is_ok()
}

fn snapshot_in_flight_claims() -> Vec<AbandonedClaim> {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect()
}

fn requeue_claims_for_live_successor(claims: &[AbandonedClaim]) -> usize {
    claims
        .iter()
        .filter(|(root, verb, task)| requeue_claim(&project_in_dir(root), verb, task))
        .count()
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
    let Ok(raw) = fs::read_to_string(handoff_inherited_claims_path()) else {
        return HashSet::new();
    };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return HashSet::new();
    };
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

const MIN_ORPHAN_CLAIM_AGE_MS: u64 = 60_000;

fn claim_age_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().ok()?.as_millis() as u64)
}

fn sweep_orphaned_claims_distinguishing_handoff_from_crash(
    root: &Path,
    inherited: &HashSet<AbandonedClaim>,
) {
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
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
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
            if in_flight_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&(root.to_path_buf(), verb.clone(), task.clone()))
            {
                continue;
            }
            let claim_age = claim_age_ms(&path);
            if claim_age
                .map(|age| age < MIN_ORPHAN_CLAIM_AGE_MS)
                .unwrap_or(false)
            {
                continue;
            }
            let out_name = format!("{verb}-{task}.json");
            let out_confirmed = if out_dir.join(&out_name).exists() {
                true
            } else {
                let out_body = serde_json::json!({
                    "ok": false,
                    "error_code": "dispatch_orphaned",
                    "reaped": true,
                    "reason": "claim file with no live dispatch in this daemon (the claiming daemon exited or was handed off) -- reaped by the periodic orphan sweep so it cannot hold the project busy",
                    "claim_age_ms": claim_age,
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that stopped answering -- a wasm trap, an out-of-memory abort, or a shared-Store recycle during the call. A version handoff is NOT a cause of this error: a handoff re-queues its claims for the incoming daemon, which completes them. The outcome is UNVERIFIED, not known to be unperformed: a side-effecting verb (git_commit/git_finalize/git_push/fs_write/memorize-fire) may already have applied some or all of its work, so read the real state (git log, git status, the file, the store) before re-dispatching. Re-dispatch straight away only for a read-only verb."),
                    "verb": verb,
                    "task": task,
                    "sweeping_pid": std::process::id(),
                }).to_string();
                let confirmed = write_spool_out_confirmed(&out_dir, &out_name, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
                confirmed
            };
            if out_confirmed {
                let _ = fs::remove_file(&path);
            } else {
                eprintln!("[agentplug daemon] could not confirm the dispatch_orphaned out-file for {verb}/{task} -- leaving the claim for the next sweep rather than deleting it unanswered");
            }
        }
    }
}

pub fn sweep_unconsumable_spool_files(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let quarantine_dir = spool_dir.join("in-quarantine");
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !file_entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if is_spool_request_path(&verb, &path) || ext == Some(ORPHAN_CLAIM_EXT) {
                continue;
            }
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name.is_empty() {
                continue;
            }
            let _ = fs::create_dir_all(&quarantine_dir);
            let dest = quarantine_dir.join(format!("{verb}__{file_name}"));
            if fs::rename(&path, &dest).is_ok() {
                eprintln!(
                    "[agentplug daemon] quarantined unconsumable spool file in/{verb}/{file_name} to {} -- the spool ABI is in/<verb>/<session-id>-<local-counter>.<ext>, so a non-conforming name is never claimed by the dispatch loop and would otherwise sit invisibly forever",
                    dest.display()
                );
            }
        }
    }
}

const RAW_PLUGIN_SPOOL_VERBS: &[&str] = &["libsql", "bert"];

fn extract_session_id(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    ["session_id", "sessionId", "SESSION_ID"]
        .into_iter()
        .filter_map(|name| value.get(name).and_then(|entry| entry.as_str()))
        .map(str::trim)
        .find(|session_id| !session_id.is_empty())
        .map(str::to_string)
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

static LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn last_measured_dispatch_queue_wait_ms() -> u64 {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn run_gm_dispatch_to_file(
    root: &Path,
    handle: &DispatchHandle,
    verb: &str,
    task: &str,
    body: &str,
    out_dir: &Path,
    queue_wait_ms: u64,
    submitted_at_ms: Option<u64>,
) {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.store(queue_wait_ms, std::sync::atomic::Ordering::Relaxed);
    let plugin_name = if RAW_PLUGIN_SPOOL_VERBS.contains(&verb) {
        verb
    } else {
        "gm"
    };
    let inner_verb_owned: String = if plugin_name == "gm" {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("verb")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "capabilities".to_string())
    };
    let tool_verb = if plugin_name == "gm" {
        verb
    } else {
        inner_verb_owned.as_str()
    };
    let _fairness_guard = GmFairnessGuard::acquire(root, tool_verb, body);
    let _tool_guard = ToolDispatchGuard::acquire(plugin_name, tool_verb, body);
    let _dispatch_origin_scope =
        agentplug_host::enter_dispatch_origin_scope(task, body, submitted_at_ms);
    let dispatch_result = if plugin_name == "gm" {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.dispatch("gm", verb, body)
        }))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.dispatch(plugin_name, &inner_verb_owned, body)
        }))
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
    let out_body = spill_large_exec_output_to_text_sibling(out_dir, verb, task, out_body);
    let out_name = format!("{verb}-{task}.json");
    let out_confirmed = write_spool_out_confirmed(out_dir, &out_name, &out_body);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    if out_confirmed {
        let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
    } else {
        eprintln!("[agentplug daemon] out-file write for {verb}/{task} did not confirm for {} -- leaving the claim for the orphan sweep instead of deleting an unanswered request", root.display());
    }
    let key: InFlightKey = (root.to_path_buf(), verb.to_string(), task.to_string());
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

fn dir_has_any_verb_subdir_with_claimable_request(base: &Path, language_stems: bool) -> bool {
    let Ok(verb_dirs) = fs::read_dir(base) else {
        return false;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        if verb_dir_has_claimable_request(&verb_entry.path(), &verb, language_stems) {
            return true;
        }
    }
    false
}

const EMPTY_VERB_DIR_VERDICT_MIN_AGE: Duration = Duration::from_secs(2);

fn empty_verb_dir_verdicts() -> &'static Mutex<HashMap<PathBuf, std::time::SystemTime>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, std::time::SystemTime>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn verb_dir_has_claimable_request(verb_dir: &Path, verb: &str, language_stems: bool) -> bool {
    let modified = fs::metadata(verb_dir)
        .and_then(|metadata| metadata.modified())
        .ok();
    if let Some(modified) = modified {
        let unchanged_since_empty_verdict = empty_verb_dir_verdicts()
            .lock()
            .ok()
            .is_some_and(|verdicts| verdicts.get(verb_dir) == Some(&modified));
        if unchanged_since_empty_verdict {
            return false;
        }
    }
    let Ok(files) = fs::read_dir(verb_dir) else {
        return false;
    };
    for file_entry in files.flatten() {
        let path = file_entry.path();
        let claimable = if language_stems {
            is_spool_request_path(verb, &path)
        } else {
            path.extension().and_then(|extension| extension.to_str()) == Some("txt")
        };
        if claimable {
            return true;
        }
    }
    if let Some(modified) = modified {
        let settled = std::time::SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= EMPTY_VERB_DIR_VERDICT_MIN_AGE);
        if settled {
            if let Ok(mut verdicts) = empty_verb_dir_verdicts().lock() {
                verdicts.insert(verb_dir.to_path_buf(), modified);
            }
        }
    }
    false
}

#[cfg(windows)]
struct IdleInDirWatch {
    entries: Vec<(PathBuf, windows_sys::Win32::Foundation::HANDLE)>,
}

#[cfg(windows)]
impl IdleInDirWatch {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn close_all(&mut self) {
        use windows_sys::Win32::Storage::FileSystem::FindCloseChangeNotification;
        for (_, handle) in self.entries.drain(..) {
            unsafe {
                FindCloseChangeNotification(handle);
            }
        }
    }

    fn sync(&mut self, roots: &[PathBuf]) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FindFirstChangeNotificationW, FILE_NOTIFY_CHANGE_DIR_NAME,
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SIZE,
        };
        let wanted: Vec<PathBuf> = roots
            .iter()
            .map(|root| root.join(".gm").join("exec-spool").join("in"))
            .filter(|dir| dir.is_dir())
            .collect();
        if self.entries.iter().map(|(p, _)| p).eq(wanted.iter()) {
            return;
        }
        self.close_all();
        for dir in wanted {
            let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
            wide.push(0);
            let handle = unsafe {
                FindFirstChangeNotificationW(
                    wide.as_ptr(),
                    1,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE,
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                self.entries.push((dir, handle));
            }
        }
    }

    const WAIT_CHUNK_HANDLES: usize = 64;

    fn wait(&self, cap: Duration) -> bool {
        use windows_sys::Win32::Storage::FileSystem::FindNextChangeNotification;
        use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
        if self.entries.is_empty() {
            std::thread::sleep(cap);
            return false;
        }
        const WAIT_OBJECT_0: u32 = 0;
        let chunks: Vec<&[(PathBuf, windows_sys::Win32::Foundation::HANDLE)]> =
            self.entries.chunks(Self::WAIT_CHUNK_HANDLES).collect();
        let per_chunk_ms = (cap.as_millis() as u64 / chunks.len() as u64).max(1);
        for chunk in chunks {
            let handles: Vec<_> = chunk.iter().map(|(_, h)| *h).collect();
            let rc = unsafe {
                WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    per_chunk_ms as u32,
                )
            };
            if (WAIT_OBJECT_0..WAIT_OBJECT_0 + handles.len() as u32).contains(&rc) {
                let idx = (rc - WAIT_OBJECT_0) as usize;
                if let Some((_, handle)) = chunk.get(idx) {
                    unsafe {
                        FindNextChangeNotification(*handle);
                    }
                }
                return true;
            }
        }
        false
    }
}

#[cfg(windows)]
impl Drop for IdleInDirWatch {
    fn drop(&mut self) {
        self.close_all();
    }
}

#[cfg(windows)]
fn wait_for_in_dir_change(watch: &mut IdleInDirWatch, roots: &[PathBuf], cap: Duration) {
    watch.sync(roots);
    if roots
        .iter()
        .any(|root| project_has_pending_dispatch_work(root))
    {
        return;
    }
    if watch.wait(cap) {
        if let Ok(mut cache) = spool_work_cache().lock() {
            cache.clear();
        }
    }
}

fn project_has_queued_spool_work(root: &Path) -> bool {
    let pd_in = root.join(".agentplug").join("plugin-dispatch").join("in");
    if let Ok(plugin_dirs) = fs::read_dir(&pd_in) {
        for plugin_entry in plugin_dirs.flatten() {
            if !plugin_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            if dir_has_any_verb_subdir_with_claimable_request(&plugin_entry.path(), false) {
                return true;
            }
        }
    }
    let gm_in = root.join(".gm").join("exec-spool").join("in");
    dir_has_any_verb_subdir_with_claimable_request(&gm_in, true)
}

const SPOOL_WORK_CACHE_TTL: Duration = Duration::from_millis(200);

fn spool_work_cache() -> &'static Mutex<HashMap<PathBuf, (Instant, bool)>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn project_has_queued_spool_work_cached(root: &Path) -> bool {
    let now = Instant::now();
    let fresh = spool_work_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(root).copied())
        .filter(|(at, _)| now.saturating_duration_since(*at) < SPOOL_WORK_CACHE_TTL)
        .map(|(_, value)| value);
    if let Some(value) = fresh {
        return value;
    }
    let value = project_has_queued_spool_work(root);
    if let Ok(mut cache) = spool_work_cache().lock() {
        cache.insert(root.to_path_buf(), (Instant::now(), value));
    }
    value
}

fn project_has_pending_dispatch_work(root: &Path) -> bool {
    project_in_flight_count(root) < MAX_CLAIMED_DISPATCHES_PER_PROJECT
        && project_has_queued_spool_work_cached(root)
}

fn dispatch_project(
    root: &Path,
    project: &mut ProjectPlugins,
    plugin_modules: &PluginModules,
) -> bool {
    let mut did_work = false;

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");

    struct ClaimedRequest {
        verb: String,
        task: String,
        body: String,
        submitted_at_ms: Option<u64>,
    }
    let mut claimed: Vec<ClaimedRequest> = Vec::new();
    let in_dir_scan = fs::read_dir(&in_dir);
    let in_dir_existed = in_dir_scan.is_ok();
    let mut claimable: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    if let (true, Ok(entries)) = (project_has_queued_spool_work_cached(root), in_dir_scan) {
        for verb_entry in entries.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(verb_entry.path()) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if !is_spool_request_path(&verb, &file_path) {
                    continue;
                }
                if !spool_in_file_write_has_settled(&file_path) {
                    continue;
                }
                let queued_since = file_entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                claimable.push((queued_since, verb.clone(), file_path));
            }
        }
    }
    claimable.sort_by_key(|(queued_since, _, _)| *queued_since);
    let claim_budget =
        MAX_CLAIMED_DISPATCHES_PER_PROJECT.saturating_sub(project_in_flight_count(root));
    for (queued_since, verb, file_path) in claimable.into_iter().take(claim_budget) {
        let Some(claim_path) = spool_claim_path(&file_path) else {
            continue;
        };
        if fs::rename(&file_path, &claim_path).is_err() {
            continue;
        }
        let task = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let body = fs::read_to_string(&claim_path).unwrap_or_default();
        if body.trim().is_empty() {
            let _ = fs::rename(&claim_path, &file_path);
            continue;
        }
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (root.to_path_buf(), verb.clone(), task.clone()),
                InFlightHandle {
                    detach: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
            );
        let submitted_at_ms = queued_since
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since_epoch| since_epoch.as_millis() as u64)
            .filter(|ms| *ms > 0);
        did_work = true;
        claimed.push(ClaimedRequest {
            verb,
            task,
            body,
            submitted_at_ms,
        });
    }

    if claimed.is_empty() && in_dir_existed && !project_has_pending_dispatch_work(root) {
        return did_work;
    }

    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }
    write_project_heartbeat(&spool_dir, read_status_busy_until_if_future(&spool_dir));

    let requested_plugins = {
        let mut list = vec!["gm".to_string()];
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
        if let Some(out_body) = session_id_task_mismatch_rejection(&req.verb, &req.task, &req.body)
        {
            write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
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
            write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
        }
    };
    for req in plugin_refresh_requests {
        let out_body = handle_plugin_refresh_request(root, &req.body);
        write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
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
                if plugin_name == "gm" {
                    gm_load_failure_reason = Some(reason);
                }
                continue;
            };
            if let Err(e) =
                project.load_plugin(&plugin_modules.engine, plugin_name, module, content_hash)
            {
                let reason = format!(
                    "failed to instantiate plugin {plugin_name} for {}: {e:#}",
                    root.display()
                );
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
                if plugin_name == "gm" {
                    gm_load_failure_reason = Some(reason);
                }
            }
        }

        if !project.is_loaded("gm") {
            let error_message = match &gm_load_failure_reason {
                Some(reason) => format!("gm plugin failed to load for this project: {reason}"),
                None => "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)".to_string(),
            };
            for req in &gm_requests {
                let out_body =
                    serde_json::json!({"ok": false, "error": error_message, "verb": req.verb})
                        .to_string();
                write_spool_out_and_release_claim(
                    &out_dir, &in_dir, &req.verb, &req.task, &out_body,
                );
            }
            answer_bg_converts(bg_convert_requests);
        } else {
            for req in gm_requests {
                let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((
                    plugin_modules.engine.clone(),
                    plugin_modules.modules_with_hashes(),
                )));
                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
                let failed_spawn_key = key.clone();
                let failed_spawn_verb = req.verb.clone();
                let failed_spawn_task = req.task.clone();
                in_flight_map()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        key.clone(),
                        InFlightHandle {
                            detach: detach_flag,
                        },
                    );

                let thread_root = root.to_path_buf();
                let thread_out_dir = out_dir.clone();
                let queue_wait_ms = req
                    .submitted_at_ms
                    .map(|submitted| now_ms().saturating_sub(submitted))
                    .unwrap_or(0);
                let spawn_result = std::thread::Builder::new()
                    .name(format!("gm-dispatch-{}", req.task))
                    .spawn(move || {
                        let _release_in_flight_entry = InFlightEntryRelease { key };
                        run_gm_dispatch_to_file(
                            &thread_root,
                            &self_healing_dispatch_handle,
                            &req.verb,
                            &req.task,
                            &req.body,
                            &thread_out_dir,
                            queue_wait_ms,
                            req.submitted_at_ms,
                        );
                    });
                if let Err(e) = spawn_result {
                    eprintln!("[agentplug daemon] could not spawn a dispatch thread for {}: {e} -- answering the request with an error instead of leaving it claimed", root.display());
                    in_flight_map()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&failed_spawn_key);
                    let out_body = serde_json::json!({"ok": false, "error": format!("daemon could not start a dispatch thread: {e}"), "verb": failed_spawn_verb}).to_string();
                    write_spool_out_and_release_claim(
                        &out_dir,
                        &in_dir,
                        &failed_spawn_verb,
                        &failed_spawn_task,
                        &out_body,
                    );
                }
            }

            answer_bg_converts(bg_convert_requests);
            write_project_heartbeat(&spool_dir, Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS));
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
    let Ok(plugin_dirs) = fs::read_dir(&pd_in) else {
        return did_work;
    };
    for plugin_entry in plugin_dirs.flatten() {
        if !plugin_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let plugin_name = plugin_entry.file_name().to_string_lossy().into_owned();
        let Ok(verb_dirs) = fs::read_dir(plugin_entry.path()) else {
            continue;
        };
        for verb_entry in verb_dirs.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(verb_entry.path()) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                    continue;
                }
                let claim_path = plugin_dispatch_claim_path(&file_path);
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                let task = file_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();

                let write_pd_out = |out_name: &str, out_body: &str| {
                    let tmp = pd_out.join(format!("{out_name}.tmp.{}", std::process::id()));
                    if fs::write(&tmp, out_body).is_ok() {
                        let _ = fs::rename(&tmp, pd_out.join(out_name));
                        let _ = fs::write(pd_out.join(format!("{out_name}.ready")), b"");
                    }
                    let _ = fs::remove_file(&claim_path);
                };

                {
                    let current = plugin_modules
                        .module_with_hash(&plugin_name)
                        .map(|(_, hash)| project.is_loaded_current(&plugin_name, hash))
                        .unwrap_or_else(|| project.is_loaded(&plugin_name));
                    if !current {
                        let Some((module, content_hash)) =
                            plugin_modules.module_with_hash(&plugin_name)
                        else {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        };
                        if let Err(e) = project.load_plugin(
                            &plugin_modules.engine,
                            &plugin_name,
                            module,
                            content_hash,
                        ) {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        }
                    }
                }

                if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(
                    &DaemonConfig::load(),
                ) {
                    let mut released: Vec<&str> = Vec::new();
                    for shared_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
                        if shared_name != plugin_name
                            && agentplug_host::release_shared_plugin(shared_name)
                        {
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

                let _tool_guard = ToolDispatchGuard::acquire(&plugin_name, &verb, &body);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    project.dispatch(&plugin_name, &verb, &body)
                }));
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
                let out_body =
                    patch_update_available_from_escalation(&plugin_name, &verb, out_body);
                write_pd_out(&out_name, &out_body);
                return true;
            }
        }
    }

    did_work
}

pub enum DaemonDispatchOutcome {
    Answered(String),
    NeverClaimedRunLocally,
    ClaimedUnanswered(String),
}

const PLUGIN_DISPATCH_CLAIM_WAIT_MS_DEFAULT: u64 = 30_000;
const PLUGIN_DISPATCH_CLAIMED_TIMEOUT_MS_DEFAULT: u64 = 20 * 60 * 1000;
const PLUGIN_DISPATCH_POLL_MS: u64 = 25;
const PLUGIN_DISPATCH_OWNER_LIVENESS_CHECK_MS: u64 = 5_000;

fn env_ms_or(name: &str, default_ms: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_ms)
}

fn plugin_dispatch_claim_path(req_path: &Path) -> PathBuf {
    req_path.with_extension(format!("txt.claim.{}", std::process::id()))
}

fn find_plugin_dispatch_claim(in_dir: &Path, task: &str) -> Option<(PathBuf, Option<u64>)> {
    let prefix = format!("{task}.txt.claim.");
    fs::read_dir(in_dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let claimer_pid = name.strip_prefix(&prefix)?.parse::<u64>().ok();
        Some((entry.path(), claimer_pid))
    })
}

fn take_plugin_dispatch_answer(out_path: &Path) -> Option<String> {
    let content = fs::read_to_string(out_path).ok()?;
    let _ = fs::remove_file(out_path);
    let _ = fs::remove_file(out_path.with_extension("json.ready"));
    Some(content)
}

fn reclaim_unclaimed_plugin_dispatch(req_path: &Path) -> bool {
    let reclaimed = req_path.with_extension(format!("txt.reclaimed.{}", std::process::id()));
    if fs::rename(req_path, &reclaimed).is_err() {
        return false;
    }
    let _ = fs::remove_file(&reclaimed);
    true
}

fn unanswered_dispatch_report(
    error_code: &str,
    error: String,
    plugin: &str,
    verb: &str,
    task: &str,
    claimer_pid: Option<u64>,
    waited_ms: u64,
    out_path: &Path,
) -> String {
    serde_json::json!({
        "ok": false,
        "error_code": error_code,
        "error": error,
        "plugin": plugin,
        "verb": verb,
        "task": task,
        "claimer_pid": claimer_pid,
        "waited_ms": waited_ms,
        "out_path": out_path.to_string_lossy(),
        "re_executed_locally": false,
    })
    .to_string()
}

pub fn try_dispatch_via_daemon(
    cwd: &Path,
    plugin: &str,
    verb: &str,
    body: &str,
) -> DaemonDispatchOutcome {
    use DaemonDispatchOutcome::{Answered, ClaimedUnanswered, NeverClaimedRunLocally};
    let cwd = agentplug_host::project_root(cwd);
    if std::env::var("AGENTPLUG_NO_DAEMON").is_ok() {
        return NeverClaimedRunLocally;
    }
    if let Err(e) = register_project(&cwd) {
        eprintln!("[agentplug] {e}");
        return NeverClaimedRunLocally;
    }
    if !ensure_daemon_running().unwrap_or(false) {
        return NeverClaimedRunLocally;
    }

    let pd_dir = cwd.join(".agentplug").join("plugin-dispatch");
    let in_dir = pd_dir.join("in").join(plugin).join(verb);
    let out_dir = pd_dir.join("out");
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return NeverClaimedRunLocally;
    }

    let task = format!("{}{}", std::process::id(), now_ms());
    let req_path = in_dir.join(format!("{task}.txt"));
    let staging_path = in_dir.join(format!("{task}.txt.staging"));
    if fs::write(&staging_path, body).is_err() || fs::rename(&staging_path, &req_path).is_err() {
        let _ = fs::remove_file(&staging_path);
        return NeverClaimedRunLocally;
    }
    let out_path = out_dir.join(format!("{plugin}-{verb}-{task}.json"));

    let claim_wait_ms = env_ms_or(
        "AGENTPLUG_DISPATCH_CLAIM_WAIT_MS",
        PLUGIN_DISPATCH_CLAIM_WAIT_MS_DEFAULT,
    );
    let claimed_timeout_ms = env_ms_or(
        "AGENTPLUG_DISPATCH_CLAIMED_TIMEOUT_MS",
        PLUGIN_DISPATCH_CLAIMED_TIMEOUT_MS_DEFAULT,
    );
    let started = Instant::now();
    let mut claimed_at: Option<Instant> = None;
    let mut last_owner_liveness_check = Instant::now();
    loop {
        if let Some(answer) = take_plugin_dispatch_answer(&out_path) {
            return Answered(answer);
        }
        let waited_ms = started.elapsed().as_millis() as u64;
        match claimed_at {
            None if !req_path.exists() => {
                claimed_at = Some(Instant::now());
                continue;
            }
            None => {
                if waited_ms >= claim_wait_ms
                    && find_plugin_dispatch_claim(&in_dir, &task).is_none()
                    && reclaim_unclaimed_plugin_dispatch(&req_path)
                {
                    eprintln!("[agentplug] daemon never claimed {plugin}/{verb} task {task} within {claim_wait_ms}ms -- reclaimed the request atomically, running it locally exactly once");
                    return NeverClaimedRunLocally;
                }
                if waited_ms >= claim_wait_ms.saturating_add(claimed_timeout_ms) {
                    return ClaimedUnanswered(unanswered_dispatch_report(
                        "fallback_reclaim_failed",
                        format!("{plugin}/{verb} task {task} was never claimed by the daemon, but the request file {} could not be atomically reclaimed for the local fallback within {waited_ms}ms -- not run locally, because the daemon could still claim it and the verb would then run twice", req_path.display()),
                        plugin, verb, task.as_str(), None, waited_ms, &out_path,
                    ));
                }
            }
            Some(at) => {
                if last_owner_liveness_check.elapsed()
                    >= Duration::from_millis(PLUGIN_DISPATCH_OWNER_LIVENESS_CHECK_MS)
                {
                    last_owner_liveness_check = Instant::now();
                    if let Some((claim_path, Some(claimer_pid))) =
                        find_plugin_dispatch_claim(&in_dir, &task)
                    {
                        if !pid_is_alive(claimer_pid) {
                            if let Some(answer) = take_plugin_dispatch_answer(&out_path) {
                                return Answered(answer);
                            }
                            let _ = fs::remove_file(&claim_path);
                            return ClaimedUnanswered(unanswered_dispatch_report(
                                "claim_owner_dead",
                                format!("daemon pid {claimer_pid} claimed {plugin}/{verb} task {task} and exited without answering -- the outcome is UNVERIFIED (a side-effecting verb may have applied some or all of its work), so it was NOT re-executed locally; read the real state (git log, the file, the store) before re-dispatching"),
                                plugin, verb, task.as_str(), Some(claimer_pid), waited_ms, &out_path,
                            ));
                        }
                    }
                }
                if at.elapsed() >= Duration::from_millis(claimed_timeout_ms) {
                    let claimer_pid =
                        find_plugin_dispatch_claim(&in_dir, &task).and_then(|(_, pid)| pid);
                    return ClaimedUnanswered(unanswered_dispatch_report(
                        "claimed_still_in_flight",
                        format!("the daemon claimed {plugin}/{verb} task {task} and has not answered within {claimed_timeout_ms}ms (AGENTPLUG_DISPATCH_CLAIMED_TIMEOUT_MS) -- it was NOT re-executed locally because the daemon may still be performing it; its answer will land at {}; read the real state before re-dispatching a side-effecting verb", out_path.display()),
                        plugin, verb, task.as_str(), claimer_pid, waited_ms, &out_path,
                    ));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(PLUGIN_DISPATCH_POLL_MS));
    }
}

fn github_cli_config_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("AGENTPLUG_GH_CONFIG_DIR") {
        candidates.push((PathBuf::from(path), "AGENTPLUG_GH_CONFIG_DIR"));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push((PathBuf::from(path).join("gh"), "XDG_CONFIG_HOME"));
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
    configure_github_cli_config_dir();

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
    HEARTBEAT_DAEMON_BOOT_TS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    write_daemon_heartbeat(0, 0);
    eprintln!(
        "[agentplug daemon] BOOT pid={} version={} ts={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        now_ms()
    );

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
    let mut project_round_robin_cursor = 0usize;
    let mut last_per_root_plugin_scan = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);

    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    const IDLE_WAIT_MIN_MS: u64 = 25;
    const IDLE_WAIT_MAX_MS: u64 = 1_000;
    let mut idle_wait_ms = IDLE_WAIT_MIN_MS;

    let shared_plugin_release_idle_ms = daemon_cfg.shared_plugin_release_idle_ms();
    let mut last_shared_release = Instant::now();

    let instruction_source_poll_interval = daemon_cfg.instruction_source_poll_interval();
    let staged_runner_rx = spawn_update_poll_worker(daemon_cfg.clone());
    #[cfg(windows)]
    let mut idle_in_dir_watch = IdleInDirWatch::new();
    let browser_orphan_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
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

    let mut last_browser_orphan_sweep = Instant::now()
        .checked_sub(Duration::from_millis(5 * 60 * 1000))
        .unwrap_or_else(Instant::now);

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
            agentplug_host::close_all_sessions();
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
        if last_browser_orphan_sweep.elapsed()
            >= Duration::from_millis(BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS)
            && !browser_orphan_sweep_in_flight.load(std::sync::atomic::Ordering::SeqCst)
        {
            last_browser_orphan_sweep = Instant::now();
            browser_orphan_sweep_in_flight.store(true, std::sync::atomic::Ordering::SeqCst);
            let finished = browser_orphan_sweep_in_flight.clone();
            let sweep_roots = known_roots.clone();
            std::thread::spawn(move || {
                agentplug_host::reap_idle_sessions_and_os_orphans_across_every_known_project_root(
                    &sweep_roots,
                );
                finished.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        }

        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

        const PER_ROOT_PLUGIN_SCAN_INTERVAL: Duration = Duration::from_secs(5);
        if !roots_new_this_registry_poll.is_empty()
            || last_per_root_plugin_scan.elapsed() >= PER_ROOT_PLUGIN_SCAN_INTERVAL
        {
            last_per_root_plugin_scan = Instant::now();
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
        agentplug_host::set_sibling_reload_source((
            plugin_modules.engine.clone(),
            plugin_modules.modules_with_hashes(),
        ));

        let sweep_cold_this_tick = last_cold_project_sweep.elapsed() >= COLD_PROJECT_SWEEP_INTERVAL;
        if sweep_cold_this_tick {
            last_cold_project_sweep = Instant::now();
            let no_inherited_claims = HashSet::new();
            for root in &known_roots {
                sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &no_inherited_claims);
            }
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
                None if sweep_cold_this_tick
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
                    write_project_heartbeat_with_queue_info(
                        &spool_dir,
                        read_status_busy_until_if_future(&spool_dir),
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
                    write_project_heartbeat_with_queue_info(
                        &spool_dir,
                        read_status_busy_until_if_future(&spool_dir),
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
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            mark_intentional_exit("heartbeat-authority-lost");
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
                crate::update_trust::remove_stage_record(&staged);
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
        if !self_update_backed_off
            && ((!any_work && !detached_still_running)
                || force_handoff_despite_in_flight
                || force_handoff_never_idle)
        {
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
        if any_work || pending_work_exists_before_idle_wait {
            idle_wait_ms = IDLE_WAIT_MIN_MS;
            #[cfg(windows)]
            wait_for_in_dir_change(
                &mut idle_in_dir_watch,
                &known_roots,
                Duration::from_millis(idle_wait_ms),
            );
            #[cfg(not(windows))]
            std::thread::sleep(Duration::from_millis(idle_wait_ms));
        } else {
            #[cfg(windows)]
            wait_for_in_dir_change(
                &mut idle_in_dir_watch,
                &known_roots,
                Duration::from_millis(idle_wait_ms),
            );
            #[cfg(not(windows))]
            std::thread::sleep(Duration::from_millis(idle_wait_ms));
            idle_wait_ms = (idle_wait_ms.saturating_mul(2)).min(IDLE_WAIT_MAX_MS);
        }
    }
}
