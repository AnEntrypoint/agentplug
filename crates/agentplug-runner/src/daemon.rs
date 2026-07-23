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

/// Live-found (vendor-daemon-lifecycle-timing-config-to-gm): every daemon-
/// lifecycle Duration below (REGISTRY_POLL_INTERVAL, HEARTBEAT_INTERVAL,
/// PLUGIN_UPDATE_POLL_INTERVAL, RUNNER_UPDATE_POLL_INTERVAL) was a hardcoded
/// const, unreachable to configure. Unlike browser-config.json (a genuine
/// per-project setting, since the browser verb operates within one
/// project's cwd), these apply machine-wide -- the daemon is a SINGLE
/// SHARED process across every registered project (confirmed via
/// shared_process:true in .status.json), so a per-project override would be
/// ambiguous: which of N registered projects' config should win for a
/// setting that governs the one shared daemon process? The correct scope is
/// install_dir() (~/.agentplug), the same machine-wide root every other
/// genuinely-shared daemon state already lives under (daemon-registry.txt,
/// daemon-owner.lock, plugins/). Read once at daemon startup (these govern
/// the daemon's OWN loop timing, not a per-dispatch value, so re-reading
/// per-tick would be wasted work); every field optional, falling back to
/// the exact pre-existing literal when absent so an unconfigured machine
/// behaves byte-identically to before this change.
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
    // How many project-worker threads run concurrently (was a hardcoded
    // `const MAX_CONCURRENT_PROJECTS: usize = 4`). Each worker pulls the
    // next root off the shared queue and may block for the duration of a
    // slow exec_js/browser dispatch, so this is the real ceiling on how
    // many projects can be mid-dispatch at once.
    #[serde(default)]
    max_concurrent_projects: Option<usize>,
    // How many concurrent calls into the shared `gm` plugin are allowed at
    // once. `gm` is genuinely stateless (its real state lives in each
    // project's own `.gm/` flat files, never in wasm memory), so more than
    // one live Store is always safe -- this just bounds how many worker
    // threads can be mid-gm-call simultaneously before the next one queues.
    // Defaults to max_concurrent_projects: a fast instruction/phase-status/
    // transition call rarely contends regardless, and a slow exec_js/browser
    // call should be able to run on every worker at once without queuing
    // behind an unrelated project's own slow call -- see is_stateless_shared_plugin's
    // doc comment in agentplug-host/src/registry.rs for the full history.
    #[serde(default)]
    gm_concurrency: Option<usize>,
    // How many live Stores exist for EACH of bert/treesitter/libsql (the
    // non-"gm" stateless-shared plugins). Defaults to 1 (byte-identical to
    // the pre-existing hardcoded behavior) -- see
    // registry.rs::SIDE_PLUGIN_POOL_SIZE's own doc comment for when raising
    // this is warranted (genuine cross-project codesearch/recall/embed
    // contention, bounded today by a 20s pool-acquire timeout rather than
    // hanging, but still real added latency under concurrent multi-project
    // load).
    #[serde(default)]
    side_plugin_concurrency: Option<usize>,
}

impl DaemonConfig {
    fn load() -> Self {
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

// 2x the daemon's own default heartbeat_interval (10s, now configurable via
// DaemonConfig, see above) plus slack, not the looser
// 60s this was originally set to -- a genuinely-alive daemon's ts is never
// more than ~10-12s stale (one missed tick at worst), so 60s left a wide
// window where a status file from a daemon killed seconds ago still reads
// as "fresh," making ensure_daemon_running() report success with no
// process actually alive. Live-witnessed this session: killed a daemon,
// immediately re-ran spool, got "registered with the shared daemon" back
// with zero agentplug-runner.exe processes running at all.
const DAEMON_STALE_MS: u64 = 20_000;

fn daemon_status_path() -> PathBuf {
    install_dir().join("daemon-status.json")
}

fn daemon_lock_path() -> PathBuf {
    install_dir().join("daemon.lock")
}

/// The single-instance ownership token. Unlike `daemon-status.json` (a plain
/// heartbeat, freely overwritten by anyone -- the exact TOCTOU this file
/// exists to close), ownership of THIS file is claimed exactly once via
/// `OpenOptions::create_new` (O_EXCL), an atomic check-and-claim the
/// filesystem itself arbitrates: of any number of processes racing the same
/// `create_new` call at the same instant, the OS guarantees exactly one
/// succeeds. Contains only the owning pid as decimal text.
fn daemon_owner_path() -> PathBuf {
    install_dir().join("daemon-owner.lock")
}

fn read_owner_pid() -> Option<u64> {
    fs::read_to_string(daemon_owner_path()).ok().and_then(|s| s.trim().parse::<u64>().ok())
}

/// Atomically claim daemon ownership for this process. Returns true iff this
/// process now holds (or already held) the owner file. Never a check-then-act
/// window: either `create_new` itself wins (filesystem-arbitrated, the only
/// non-racy primitive here) or a stale owner is replaced via a tmp-write +
/// atomic rename (also filesystem-arbitrated -- `rename` is atomic on both
/// Windows and POSIX, so a second process attempting the same takeover at the
/// same instant still can't produce a torn/mixed owner file, only a clean
/// last-writer-wins). The freshness signal for "stale" is the SEPARATE
/// heartbeat file's ts (a lock file has no ts of its own), so a takeover only
/// fires once the previous owner has demonstrably stopped heartbeating for
/// DAEMON_STALE_MS, not merely "some other pid's lock file exists."
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

    // Owner file already exists. Only ever take over a STALE one -- staleness
    // is judged by the heartbeat file, never by the lock file's own presence,
    // so a live owner (heartbeating fine, but hasn't rewritten its own already-
    // correct owner file) is never mistaken for dead. Timestamp freshness
    // ALONE is not enough (the same class of race is_daemon_fresh's own fix
    // closed): a process that just died leaves a heartbeat that reads as
    // fresh for up to DAEMON_STALE_MS with nothing alive behind it -- verify
    // the pid is a real live process too, live-hit this session as a direct
    // `daemon` boot losing its own claim to a pid that no longer existed.
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

    // Stale: replace via tmp-write-then-atomic-rename, never remove+create
    // (a remove+create window is exactly the TOCTOU gap this file exists to
    // close -- a second stale-takeover attempt landing in that gap would
    // itself race a plain create_new). rename() itself is the atomic
    // publish; whichever racing takeover renames last simply wins cleanly,
    // no torn intermediate state observable by a third reader.
    let tmp_path = owner_path.with_extension(format!("lock.tmp.{my_pid}"));
    if fs::write(&tmp_path, my_pid.to_string()).is_err() {
        return false;
    }
    if fs::rename(&tmp_path, &owner_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return false;
    }
    // Re-read after the rename: another process's takeover may have
    // rename()'d over ours a moment later. Only proceed if we're still the
    // recorded owner post-publish.
    read_owner_pid() == Some(my_pid)
}

/// True when this process currently holds (or has just reclaimed) ownership.
/// Shared by the heartbeat-tick re-check and the pre-work check at the top of
/// the poll loop so both decide identically -- a process that ever reads a
/// DIFFERENT live pid in the owner file has lost authority and must exit
/// before doing further work, never merely "before writing its next
/// heartbeat."
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

/// A daemon killed via `Stop-Process -Force`/`taskkill /F`/SIGKILL leaves no
/// chance to run cleanup code, so `daemon-status.json`'s last heartbeat can
/// read as "fresh" (within DAEMON_STALE_MS) for up to that whole window even
/// though the process is already gone -- narrowing the window (60s -> 20s,
/// see DAEMON_STALE_MS's own history) only shrinks the race, it can't close
/// it. Live-hit this session even at 20s: kill the daemon, immediately
/// re-run spool, get "registered with the shared daemon" back with the pid
/// from the stale status file no longer existing as a real process. Since
/// daemon-status.json already carries its writer's own pid, verify it
/// against the real process table before trusting the timestamp -- a
/// timestamp alone can never distinguish "still alive" from "died a moment
/// ago, heartbeat just hasn't gone stale yet." Only called on the already-
/// slow boot/register path (never the hot per-dispatch path), so the extra
/// process-table query cost is negligible here.
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
            // tasklist prints a matching CSV row ("name","pid",...) when the
            // pid exists, or an "INFO: No tasks..." line (no comma) when it
            // does not -- a comma in the first line is the cheap discriminator.
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        // tasklist itself failing to run is a host-environment problem, not
        // evidence the daemon is dead -- fail open (trust the timestamp
        // alone, the pre-existing behavior) rather than false-negative every
        // boot attempt on a host where tasklist is unavailable/blocked.
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u64) -> bool {
    // kill(pid, 0) checks existence/permission without sending a real signal
    // -- the POSIX-standard liveness probe, zero new dependencies needed.
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

/// Per-project instruction-source configuration: `.gm/instructions/source.json`
/// opts a project INTO pulling its phase prose from a git repo other than
/// the compiled-in default (this repo, gm/AnEntrypoint) -- e.g. an org-wide
/// shared instruction set, or a project's own fork of the prose files.
/// `{"repo": "https://...", "branch": "main", "path": "instructions/"}`.
/// Absence of this file means the project uses only the compiled default and
/// its own local .gm/instructions/<key>.md overrides -- no new behavior,
/// pure opt-in.
#[derive(serde::Deserialize)]
struct InstructionSourceConfig {
    repo: String,
    #[serde(default = "default_branch")]
    branch: String,
    // Not read here -- this struct only validates + drives the clone/fetch
    // (repo+branch). `path` (which subdir within the repo prose lives in)
    // is read independently, wasm-side, by prose::read_from_source_repo at
    // actual resolve time. Kept here anyway so a project's source.json with
    // a `path` field that doesn't match this schema still parses cleanly
    // (an unknown-to-THIS-struct field would otherwise be silently ignored
    // by serde regardless, but declaring it documents the full config shape
    // in one place rather than splitting it silently across two crates).
    #[allow(dead_code)]
    #[serde(default)]
    path: String,
}
fn default_branch() -> String { "main".to_string() }

fn instruction_source_config_path(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions").join("source.json")
}

/// Where a project's synced source-repo prose lands -- INSIDE that project's
/// own .gm/ tree (not the global install_dir()), so a project's chosen
/// source is scoped to that project, never leaked across projects sharing
/// this one daemon, and travels with the project if .gm/ is itself synced
/// elsewhere (though the managed-gitignore block excludes this cache dir
/// from being committed, same treatment as any other transient runtime
/// artifact -- it is a CACHE of the source repo, not the source of truth).
fn instruction_source_cache_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions-source-cache")
}

/// Reads a project's source.json (if present) and keeps
/// instruction_source_cache_dir(root) in sync with it via plain `git`
/// subprocess calls -- clone on first sight, fetch+reset thereafter. Shells
/// out to the real git binary (same discipline as rs-plugkit's own git
/// verbs) rather than adding a git library dependency; git is already a
/// hard runtime requirement everywhere this daemon runs (gm's own git verbs
/// need it). A missing/unparseable source.json is not an error -- most
/// projects will never opt into this, so silently doing nothing is correct,
/// not a fallback path worth logging.
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
    // Already cloned -- fetch + hard-reset to the branch tip rather than
    // pull/merge, since this is a read-only mirror the daemon owns
    // exclusively (never has local commits of its own to preserve), and a
    // hard reset is the one operation that can never produce a merge
    // conflict / diverged-history failure mode on an unattended sync.
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

/// Called by an OLD daemon's main loop (see PLUGIN_UPDATE_POLL_INTERVAL's
/// sibling poll below) once `download::stage_runner_self_update()` has
/// staged a verified `<exe>.new`. Spawns the staged binary as `takeover
/// <version>`, waits (bounded) for it to prove itself alive and about to
/// serve, then VOLUNTARILY releases ownership and returns true (caller exits
/// the process immediately after) -- a real handoff, never a race-to-
/// staleness. Returns false (caller keeps running unchanged) if the new
/// process never proves itself in time, so a broken build can never take
/// down the one daemon everyone depends on; the stale `.new` is simply
/// retried next poll (harmless -- download_and_verify's tmp-then-rename
/// means a half-written `.new` from an interrupted previous attempt is never
/// left in a state this could pick up broken).
fn attempt_self_update_handoff(staged_exe: &Path, version: &str) -> bool {
    let ready_path = takeover_ready_path();
    let _ = fs::remove_file(&ready_path);
    if spawn_detached(staged_exe, &["takeover", version]).is_err() {
        return false;
    }
    // Bounded wait for the new process's readiness marker -- generous
    // relative to a cold boot (engine build + 4 default plugin compiles,
    // measured this session at several-hundred-ms to low-seconds) but never
    // unbounded: a hung/crash-looping new binary must not stall the old
    // daemon's own loop (heartbeat authority re-check, dispatch service)
    // past the point where ITS OWN heartbeat would go stale and get
    // reclaimed by yet another process.
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

/// Deletes this process's own owner-lock file (only if it is genuinely still
/// the recorded owner -- never blind-deletes a file another process may have
/// already taken over) so the new process's own claim_ownership() succeeds
/// immediately via the create_new fast path, rather than needing to wait out
/// DAEMON_STALE_MS for a takeover-via-staleness. A true release, not a crash.
fn release_ownership_for_handoff() {
    let my_pid = std::process::id() as u64;
    if read_owner_pid() == Some(my_pid) {
        let _ = fs::remove_file(daemon_owner_path());
    }
}

/// Entry point for the `takeover <version>` subcommand (main.rs) -- run by
/// the NEWLY STAGED binary the old daemon just spawned. Builds a real engine
/// and compiles the default plugin set (the same cost run_daemon() itself
/// pays) BEFORE writing the readiness marker, so "ready" genuinely means
/// "can serve," not merely "process started." Only after the old daemon
/// observes readiness and releases ownership does this process claim it and
/// fall into the exact same run_daemon() loop a normal boot would -- from
/// that point on it IS the daemon, indistinguishable from one that started
/// the ordinary way.
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
    // Wait for the OLD process to actually release (not just for our own
    // claim_ownership() to succeed against a file that might still exist
    // mid-delete) -- polling read_owner_pid()==None avoids a window where we
    // "claim" by racing a rename the old process is still mid-write on.
    // Generous window (2 minutes, not the original 10s): live-witnessed this
    // session that the OLD daemon's own release runs from inside its main
    // loop, which can be blocked well past a few seconds on one long
    // synchronous wasm call already in flight when the update poll fires --
    // the exact same "stuck in one long call, misses its own heartbeat tick"
    // condition documented at holds_heartbeat_authority's call site above. A
    // takeover that gives up in 10s under that same realistic load would
    // abandon a handoff that was genuinely still coming, forcing a full
    // extra poll-interval wait (up to RUNNER_UPDATE_POLL_INTERVAL) before
    // the next attempt -- costing far more than patiently waiting out one
    // busy dispatch actually would have.
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

/// Live count of registered projects, updated by the main loop after every
/// `thread::scope` batch completes -- read by the heartbeat ticker thread so
/// its independently-written heartbeats still carry a real (if momentarily
/// lagging) `active_projects` figure instead of a permanently-stale 0.
static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set once by the main loop when the heartbeat ticker thread (see
/// `spawn_heartbeat_ticker`) observes that authority has been lost to another
/// process -- the ticker itself can never safely call `close_all_sessions()`
/// or return from `run_daemon_body` (those touch/own state the MAIN thread's
/// loop owns), so it only raises this flag; the main loop polls it cheaply at
/// the top of every iteration (and right after every dispatch batch) and
/// performs the actual shutdown itself. `Relaxed` is sufficient: this is a
/// single one-way latch (false->true, never reset within a process lifetime),
/// not synchronizing access to any other data.
static HEARTBEAT_AUTHORITY_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn heartbeat_authority_lost() -> bool {
    HEARTBEAT_AUTHORITY_LOST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Fix for the real liveness bug documented at this function's former single
/// call site (now split, see below): the heartbeat write and the authority
/// re-check used to run ONLY at the top of the main loop, which cannot return
/// there until the current tick's `thread::scope` block has joined every
/// worker thread -- so one worker occupying a slot for up to
/// `DISPATCH_CALL_DEADLINE_SECS` (40s) delayed the heartbeat write for that
/// same duration, exactly the class of incident the doc comment on the old
/// single call site described (a daemon stuck mid-call 12+ seconds without
/// heartbeating while burning a full core, until a competing daemon claimed
/// authority and took over).
///
/// This spawns a genuinely independent OS thread that owns heartbeat timing
/// entirely -- it never touches `projects`/`plugin_modules`/worker threads,
/// only the filesystem (`write_daemon_heartbeat`) and the same
/// filesystem-arbitrated ownership primitives (`holds_heartbeat_authority`)
/// the old inline check used, both of which were already safe to call from
/// any thread (no shared mutable state beyond what the OS file operations
/// themselves already arbitrate atomically). On losing authority it raises
/// `HEARTBEAT_AUTHORITY_LOST` and returns -- the main loop notices on its own
/// next check and performs the actual (session-owning) shutdown.
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

/// One compiled Module per DISTINCT plugin name, shared across every
/// project -- the expensive Cranelift compile happens once regardless of
/// how many projects use the "bert" plugin. Instantiation (cheap) happens
/// per (project, plugin) inside ProjectPlugins.
struct PluginModules {
    engine: Engine,
    modules: HashMap<String, Module>,
}

impl PluginModules {
    fn new() -> anyhow::Result<Self> {
        Ok(Self { engine: build_engine()?, modules: HashMap::new() })
    }

    /// Split borrow (engine immutable, modules mutable) so a caller holding
    /// the returned `&Module` can still separately borrow `self.engine` --
    /// `get_or_compile(&mut self) -> &Module` would tie the Module borrow to
    /// the whole struct, making `plugin_modules.engine` inaccessible while
    /// the returned Module reference is still alive at the call site.
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

/// Key identifying one in-flight gm-spool dispatch: the project root, the
/// verb name, and the request's own numeric filename stem (the same "task
/// id" the calling agent already knows, since it wrote `in/<verb>/<task>.txt`
/// itself). Unique across the whole daemon process -- two different projects
/// or two different verbs never collide even if their task-id timestamps
/// happen to coincide.
type InFlightKey = (PathBuf, String, String);

/// Per-in-flight-dispatch state the `background-convert` verb flips.
/// `AtomicBool` (not a plain bool behind the outer Mutex) so the worker's
/// poll loop can check it without re-locking IN_FLIGHT on every poll tick --
/// only the registry insert/remove itself needs the outer lock.
struct InFlightHandle {
    detach: Arc<std::sync::atomic::AtomicBool>,
}

/// Process-wide registry of gm-spool dispatches currently running on their
/// own spawned thread, keyed by `(root, verb, task)`. An entry exists from
/// the moment a request's spawned thread starts until either (a) the worker's
/// poll loop observes the thread finished and removes it (normal fast path),
/// or (b) a `background-convert` fires, at which point the worker's poll loop
/// removes it itself right after flipping `detach` (the thread is now fully
/// independent -- nothing needs to find it again, it writes its own response
/// file when done). `background-convert`'s job is purely "does this key exist
/// and if so flip its flag," so the registry only ever needs to answer
/// membership + flag-flip, never anything about the thread itself.
static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Native (host-side, never routed to gm.wasm) handler for the
/// `background-convert` verb. Body: `{"verb": "<original verb>", "task":
/// "<original request's numeric filename stem>"}`. Looks the key up in
/// IN_FLIGHT for the CALLING project's own root (background-convert can only
/// ever target a dispatch belonging to the same project it's dispatched
/// against, matching the ABI's existing per-project spool-dir scoping) and,
/// if found, flips its detach flag and removes it from the registry -- the
/// worker thread waiting on that dispatch observes the flag on its next poll
/// tick (bounded by the poll interval, not blocking on the original
/// dispatch's own duration) and abandons its join, returning to the queue
/// immediately. Responds fast either way: this function never blocks on the
/// original dispatch's own completion.
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
            // Either genuinely never existed (wrong verb/task/root) or
            // already finished (the worker's poll loop already joined it and
            // removed the entry before this request arrived) -- both read as
            // "nothing left to convert," which is the correct, clear,
            // non-crashing answer either way per the confirmed spec's race
            // handling (item 4).
            serde_json::json!({"ok": false, "error": "already_completed", "verb": req.verb, "task": req.task}).to_string()
        }
    }
}

/// Runs one gm-spool dispatch (`plugin.dispatch("gm", verb, body)`, wrapped
/// in the same `catch_unwind` + `GmFairnessGuard` protection the previous
/// inline call had) to completion and writes its response to the standard
/// `out/<verb>-<task>.json` (+ `.ready`) path -- identical output shape
/// whether this runs synchronously-joined by the original worker or fully
/// detached after a `background-convert`. Takes a `DispatchHandle` (not
/// `&mut ProjectPlugins`) specifically so it can be moved onto its own OS
/// thread: see `ProjectPlugins::dispatch_handle`'s doc comment for why this
/// is `Send + 'static`-safe without needing `unsafe`.
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

/// Drains one project's ENTIRE pending spool work (both the gm spool ABI
/// and the generic plugin-dispatch surface) fully sequentially, one request
/// file at a time, in the exact claim-rename -> read -> dispatch -> respond
/// order the single-threaded loop used before per-project threading was
/// added -- a project's own dispatches are never reordered or run
/// concurrently against EACH OTHER, only run() calling this for up to
/// MAX_CONCURRENT_PROJECTS different roots on different threads makes
/// DIFFERENT projects overlap. Returns true iff at least one request file
/// was actually processed (feeds the outer loop's any_work bookkeeping for
/// heartbeat/idle-eviction/self-recycle timing, unchanged from before).
///
/// Each gm-spool request's actual `project.dispatch("gm", ...)` call now
/// runs on its OWN spawned thread from the moment it starts (never inline on
/// this function's own thread) -- this worker then does a bounded-poll join
/// (`JoinHandle::is_finished()`, ~50ms sleep between checks) instead of a
/// blocking `.join()`. On ordinary fast completion this is functionally
/// identical to the old inline call (same total wall time, negligible poll
/// overhead) and the response is written exactly as before. If a
/// `background-convert` request flips this dispatch's `detach` flag before it
/// finishes, this loop notices on its next poll tick, removes the IN_FLIGHT
/// entry, and returns WITHOUT joining -- the spawned thread keeps running
/// completely independently (Rust threads run to completion regardless of
/// whether/when anything joins them) and writes its own response file via
/// `run_gm_dispatch_to_file` when it eventually finishes. This is what lets
/// the worker return to the outer queue-pulling loop immediately on
/// background-convert, and lets a SECOND call into `dispatch_project` for
/// this SAME root (next time some worker pulls it off the queue) proceed
/// concurrently against the still-running detached dispatch -- both go
/// through the same `ProjectPlugins`/pool-slot-checkout machinery
/// (`SharedPluginPool`/`GmFairnessGuard`), which already supports genuine
/// concurrent checkouts for one project (see their own doc comments).
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
            // Default (no .agentplug/plugins.txt): "gm" plus its own real
            // runtime dependencies, not "gm" alone -- see the historical note
            // this comment carried before the per-project threading refactor:
            // gm.wasm calls host_plugin_call("libsql"/"bert"/"treesitter", ...)
            // unconditionally from inside its own recall/memorize/codesearch/
            // embed code paths, so loading only "gm" left every one of those
            // calls hitting "plugin_not_loaded_yet".
            list.push("gm".to_string());
            list.push("libsql".to_string());
            list.push("bert".to_string());
            list.push("treesitter".to_string());
        }
        list
    };

    // Claim EVERY request file across EVERY verb dir up front, in one pass,
    // before spawning or polling anything -- this is the fix for a real bug
    // live-witnessed this session: the previous shape claimed one file, then
    // ran an inline bounded-poll loop on it (blocking THIS iteration of the
    // outer for-loop) before moving to the NEXT verb_entry/file_entry. A
    // `background-convert` targeting an exec_js request sitting in a LATER
    // verb directory (verb dirs are not claimed in a fixed order --
    // fs::read_dir order is unspecified) could sit unclaimed for the ENTIRE
    // duration of that inline poll loop, since the claim-rename for
    // background-convert's own file never even runs until this dispatch_project
    // call reaches that verb_entry. Measured: exec_js (20s sleep) and a
    // background-convert targeting it, written ~300ms apart, both resolved
    // within the same ~50ms window ~80s later -- background-convert's
    // response (`already_completed`) landed AFTER exec_js's real result,
    // proving it was never claimed until the NEXT dispatch_project sweep for
    // this root, defeating the whole "does not hold up the queue" goal. Fix:
    // claim-and-collect every file first (fast, no blocking work happens
    // during collection), answer every `background-convert` immediately from
    // that same collected batch (so it can target ANY task claimed in this
    // same batch, including one collected moments earlier in this same
    // pass), THEN spawn every gm-verb dispatch, THEN poll all of them
    // together -- background-convert requests are handled before any gm
    // dispatch's poll loop can block anything.
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
                // Atomic claim: rename the request file to a pid-suffixed
                // claim name BEFORE reading it -- see the single-instance
                // ownership-claim doc comment on run_daemon for why rename()
                // is the only non-racy primitive here (unchanged from
                // before per-project threading: now it ALSO closes the
                // window between this thread and any other thread in this
                // same process touching the same root, though in practice
                // each root is only ever handed to one thread per chunk).
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

    // Split the claimed batch into gm-verb requests and background-convert
    // requests, WITHOUT answering background-convert yet -- see the critical
    // ordering fix below the plugin-load block for why.
    let mut gm_requests: Vec<ClaimedRequest> = Vec::with_capacity(claimed.len());
    let mut bg_convert_requests: Vec<ClaimedRequest> = Vec::new();
    for req in claimed {
        if req.verb == "background-convert" {
            bg_convert_requests.push(req);
        } else {
            gm_requests.push(req);
        }
    }

    // Answers a batch of background-convert requests by writing their
    // response files -- factored out since it must run from BOTH the
    // "gm not loaded" early-exit path and the normal spawn path below, and
    // (when gm_requests is empty) with no gm-verb branch at all. Always
    // called AFTER any IN_FLIGHT registration for this same batch's
    // gm_requests has already happened (see call sites), so a
    // background-convert targeting a request claimed in this exact same
    // batch finds a real entry instead of racing its own registration.
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
        // No gm-verb requests in this batch (this call only claimed
        // background-convert file(s), e.g. one targeting a dispatch spawned
        // in a PREVIOUS dispatch_project call for this same root -- already
        // either finished or still running independently after its own
        // earlier detach). Nothing to register first; answer directly.
        answer_bg_converts(bg_convert_requests);
    } else {
        // Load every plugin this project has requested that isn't already
        // loaded -- gm dispatches to "gm" by default; a project opting into
        // libsql/bert/treesitter (via .agentplug/plugins.txt) gets those
        // instantiated too, so gm.wasm's own host_plugin_call/host_vec_embed
        // finds them. Done once for the whole batch, not per request.
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

        // The "gm" plugin is the dispatch entrypoint for the existing spool
        // ABI -- fail loud with the real reason instead of a dispatch that
        // was always going to fail if "gm" itself never loaded (network
        // hiccup, plugin not yet published for this platform).
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
            // No IN_FLIGHT entries were ever registered for this batch's
            // gm_requests (they failed before spawning), so answer directly
            // -- same as the empty-gm_requests case above.
            answer_bg_converts(bg_convert_requests);
        } else {
            // Spawn EVERY gm-verb request in the batch onto its own thread,
            // registering its IN_FLIGHT entry BEFORE the thread is even
            // spawned -- critical ordering fix for a real race live-witnessed
            // this session: a `background-convert` claimed in THIS SAME
            // batch must never be answered before the request it targets
            // has an IN_FLIGHT entry to find, or it reads "already_completed"
            // for a dispatch that hasn't even started yet (a false negative
            // indistinguishable from the genuine already-finished case, but
            // for the opposite reason -- live-witnessed repeatedly this
            // session: exec_js and a background-convert targeting it,
            // written under 150ms apart, landing in the SAME claimed batch,
            // background-convert answered "already_completed" after only
            // ~100ms even though the exec_js dispatch went on to genuinely
            // run for its full real duration afterward). Registering the map
            // entry here, synchronously, before bg_convert_requests is ever
            // answered below, closes that window entirely: by the time ANY
            // background-convert in this batch is answered, every
            // gm_request's IN_FLIGHT entry already exists, regardless of
            // claim order within the batch.
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

            // NOW answer every background-convert claimed in THIS batch --
            // every gm_request above already has its IN_FLIGHT entry
            // (inserted synchronously before its thread was even spawned),
            // so a background-convert targeting a request claimed in this
            // exact same batch finds it reliably, never racing its own
            // registration.
            answer_bg_converts(bg_convert_requests);

            // Round-robin bounded-poll: `is_finished()` (stable since Rust
            // 1.61) is non-blocking, so every spawned request in this batch
            // gets checked on the same ~50ms cadence regardless of position
            // -- request 2 is no longer starved behind request 1's own poll
            // loop the way the previous single-request-inline shape starved
            // it. Loop drains until every entry has either been joined
            // (finished normally) or detached (background-convert fired for
            // it) -- whichever comes first, per entry, independently.
            //
            // Also re-scans in_dir on EVERY tick of this same loop -- not
            // just the one snapshot taken before this batch was spawned.
            // Live-witnessed bug this fixes for background-convert
            // specifically: a `background-convert` request written to disk
            // AFTER this dispatch_project call already took its initial
            // fs::read_dir snapshot (pass 1 above) is invisible to that
            // snapshot. Without re-scanning here, THIS call keeps polling its
            // own spawned batch to completion/timeout with no way to ever
            // notice that later-arriving file -- the earliest it could be
            // seen is the NEXT dispatch_project call for this root, which
            // cannot start until this one returns, by which point a fast
            // dispatch has usually already finished (measured: multiple
            // end-to-end runs where background-convert consistently reported
            // "already_completed" because it was answered a whole
            // dispatch_project-call-cycle late). Re-scanning here means a
            // background-convert arriving any time during this same
            // dispatch_project call is claimed and answered within one poll
            // tick (~50ms), while the batch's own dispatches are still
            // mid-flight, which is what actually delivers "converts fast,
            // does not wait for the dispatch."
            //
            // Generalized to every OTHER gm-verb dir too, not just
            // background-convert -- the identical starvation shape applies
            // to an ordinary request (e.g. `phase-status`) written to this
            // SAME project's spool while a slow, unrelated dispatch (e.g.
            // `codesearch`/`exec_js`) from the initial claim-snapshot is
            // still mid-flight: without this, that new request sits
            // unclaimed on disk until the NEXT dispatch_project call for
            // this root, which (per the heartbeat-decoupling fix above)
            // could itself be delayed behind this very batch. Newly-claimed
            // ordinary requests are spawned into this SAME `spawned` vector
            // (their IN_FLIGHT entry registered first, matching the
            // ordering fix above) so they get the identical round-robin
            // poll treatment as the batch's original members -- serviced
            // within this call, never starved behind an unrelated slow
            // sibling in the same batch.
            let bg_convert_dir = in_dir.join("background-convert");
            while spawned.iter().any(|s| s.join_handle.is_some()) {
                for s in spawned.iter_mut() {
                    let Some(jh) = s.join_handle.as_ref() else { continue };
                    if jh.is_finished() {
                        let jh = s.join_handle.take().unwrap();
                        let _ = jh.join();
                        in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&s.key);
                    } else if s.detach_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        // background-convert fired for this entry (already
                        // answered below/in pass 1, and already removed from
                        // IN_FLIGHT by handle_background_convert itself): do
                        // NOT join. Drop the JoinHandle without joining --
                        // the spawned thread keeps running fully
                        // independently (it already owns everything it
                        // needs: root, dispatch handle, verb, body, out_dir)
                        // and writes its own response file via
                        // run_gm_dispatch_to_file when it eventually
                        // finishes. Rust threads run to completion
                        // regardless of whether/when anything joins them, so
                        // this is safe.
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

                    // Generalized re-scan: every OTHER verb dir under in_dir
                    // (never background-convert, already handled above, and
                    // never re-reading verb dirs that don't exist yet) gets
                    // the same treatment -- claim any newly-arrived request,
                    // register its IN_FLIGHT entry, spawn its own thread, and
                    // fold it into `spawned` so the SAME while-loop condition
                    // above continues polling it alongside the batch's
                    // original members. This only needs `project` (already
                    // exclusively held by this worker thread for this root)
                    // and requires "gm" to already be loaded, which it is --
                    // this whole else-branch only runs once gm_requests was
                    // non-empty and gm loaded successfully.
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

    // Generic plugin+verb dispatch surface, separate from the gm spool
    // above -- see the original doc comment (preserved in git history at
    // this refactor's parent commit) for why it exists: a host outside
    // agentplug's own wasm graph reaching a PERSISTENT stateful plugin
    // instance (libsql) that a one-shot per-call subprocess could never
    // provide.
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

                // catch_unwind, matching run_gm_dispatch_to_file's existing
                // protection: an epoch-interrupt trap can surface as a Rust
                // panic from deep inside a host-import callback (e.g.
                // write_guest_bytes's plugkit_alloc call, itself invoked
                // while the guest's OWN deadline has already elapsed) rather
                // than a clean Err from project.dispatch's top-level Result --
                // wasmtime's trap unwinds through whatever Rust frame is on
                // the stack when the epoch check fires, which is not always
                // the outermost dispatch_fn.call. Without this, that panic
                // was silently swallowing the worker thread inside
                // thread::scope (the `let _ = h.join();` above discards a
                // panicked handle's Err), never reaching the client as any
                // response at all -- this project's dispatch loop for THIS
                // request then hung the caller until try_dispatch_via_daemon's
                // own 30s poll timeout, even though the daemon process itself
                // survived and kept serving other work.
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

/// Bounded wait for the daemon to answer a generic plugin+verb dispatch --
/// `None` means "no daemon reachable within budget", the caller's cue to
/// fall back to a standalone one-shot instantiate. Never blocks
/// indefinitely: ensure_daemon_running itself already bounds the
/// spawn-and-confirm-alive wait, and the response poll below has its own
/// separate bound so a daemon that's alive but wedged on this specific
/// project/plugin doesn't hang the caller forever either.
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
    // Timed out waiting for a response -- clean up the never-picked-up (or
    // still-processing) request so a stale file doesn't confuse a later
    // fallback attempt; the daemon's own removal-on-read makes this a
    // harmless no-op if it already claimed the file.
    let _ = fs::remove_file(&req_path);
    None
}

pub fn run_daemon() -> anyhow::Result<()> {
    eprintln!("[agentplug daemon] starting, registry {}", registry_path().display());

    // Atomic single-instance claim, BEFORE any shared plugin state (engine
    // build, wasm compile, plugin Store/libsql open) exists. Fully replaces
    // the former check-then-act heartbeat-timestamp race: that scheme read
    // daemon-status.json, decided "no fresh owner", and only THEN built an
    // Engine (build_engine, non-trivial wall time) and wrote its own first
    // heartbeat -- leaving a real window where a second process's identical
    // read-then-decide landed before the first's heartbeat write ever
    // happened, so both passed the check and both proceeded to build
    // engines, load plugins, and open libsql (live-witnessed this session:
    // two agentplug-runner.exe processes, equal memory, one with stale-but-
    // real heartbeat history). `claim_ownership()` is filesystem-arbitrated
    // (O_EXCL create, or atomic rename for stale takeover) -- there is no
    // read-then-decide gap for a second process to land in. A losing process
    // exits HERE, before `PluginModules::new()` (the Engine build) even
    // runs, so it never opens a plugin Store or a libsql DB at all.
    if !claim_ownership() {
        let existing_pid = read_owner_pid();
        eprintln!(
            "[agentplug daemon] lost the atomic ownership claim -- pid {:?} already owns the shared daemon, exiting before touching any shared plugin state",
            existing_pid
        );
        return Ok(());
    }

    let plugin_modules = PluginModules::new()?;
    // A fresh normal boot (never a self-update takeover) has no version
    // marker to preserve -- record the version this exe actually is now,
    // matching run_takeover's own record_runner_version call, so
    // installed_runner_version() is never left stale/absent after a plain
    // (non-update-triggered) start.
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
    // Must run before the first `gm` dispatch/load_plugin call anywhere in
    // this process -- the pool size is a OnceLock internal to
    // agentplug-host's registry module, so this is the one and only place
    // it is ever configured. A call after some other path already
    // lazily-initialized the pool at its 4-slot default is a harmless no-op
    // (set_gm_pool_size returns false); this is the very first thing
    // run_daemon_body does, before any project registry read or plugin
    // load, so that race is not reachable in practice.
    agentplug_host::set_gm_pool_size(daemon_cfg.gm_concurrency());
    agentplug_host::set_side_plugin_pool_size(daemon_cfg.side_plugin_concurrency());

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    // Was `Instant::now() - registry_poll_interval` (fire the first poll immediately by
    // pre-dating the timer) -- underflows/panics on every boot, since Instant::now() at
    // process start is necessarily younger than registry_poll_interval since the process's
    // own monotonic-clock epoch, live-reproduced this session. first_registry_poll_pending
    // preserves the original immediate-first-poll intent without any Instant arithmetic
    // that could underflow.
    let mut last_registry_poll = Instant::now();
    let mut first_registry_poll_pending = true;
    let mut known_roots: Vec<PathBuf> = Vec::new();

    // WASM linear memory is architecturally monotonic (memory.grow is the
    // only size-changing instruction, no shrink exists in the spec) -- once
    // a shared plugin's Store (gm/bert/treesitter/libsql, see
    // is_stateless_shared_plugin) grows to accommodate a peak allocation
    // (live-witnessed: ~1.8GB during the FIRST real codesearch's full-repo
    // codeinsight index+embed pass, candle's BertModel::forward allocating
    // batched activation tensors), that memory is retained by THIS PROCESS
    // for its entire lifetime -- there is no in-place reclamation, and
    // PLUGIN_IDLE_EVICT_MS only removes per-project ProjectPlugins map
    // entries, never touches the SHARED_PLUGINS static's actual Stores.
    // The only way to reclaim a peak is to exit this process entirely and
    // let a fresh one re-earn a low baseline -- safe because `spool`'s
    // ensure_daemon_running() always spawns a new daemon on next real need
    // (main.rs), so a clean self-exit here is a respawn, not an outage.
    // Gated on genuine full-fleet idleness (every registered project's last
    // dispatch older than SELF_RECYCLE_IDLE_MS) so an in-flight or
    // soon-to-resume project is never interrupted mid-use.
    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    // Far shorter than the whole-process recycle above: releasing one shared
    // Store costs a single re-instantiation on next use, so it can run on a
    // burst-quiet cadence rather than an hour. See the release site below for
    // the measured numbers this exists to reclaim.
    const SHARED_PLUGIN_RELEASE_IDLE_MS: u64 = 2 * 60 * 1000;
    let mut last_shared_release = Instant::now();

    let plugin_update_poll_interval = daemon_cfg.plugin_update_poll_interval();
    let mut last_plugin_update_poll = Instant::now();

    // Same cadence as the wasm-guest poll -- the runner's own executable
    // deserves no less frequent an update check than the plugins it hosts.
    // Per fsm-framework/runner-self-update: closes the "agent had to
    // manually kill/rebuild/redeploy the daemon" gap this session hit
    // repeatedly for daemon.rs fixes.
    let runner_update_poll_interval = daemon_cfg.runner_update_poll_interval();
    let mut last_runner_update_poll = Instant::now();
    // Carries a staged-but-not-yet-handed-off self-update across ticks -- see
    // the split staging/handoff block below for why this must survive a busy
    // tick instead of being re-derived each time.
    let mut pending_self_update: Option<(PathBuf, String)> = None;
    // Same split as pending_self_update above, one level down: a plugin
    // refresh whose download has already completed (verified wasm sitting on
    // disk with an updated .version file) but whose live-swap (evicting the
    // cached Module + releasing the shared Store) hasn't happened yet because
    // the tick it finished on wasn't idle. Persists across ticks so the swap
    // still lands on the first idle tick that follows, rather than being
    // silently skipped (the old all-or-nothing gate below never even
    // reached the swap on a busy tick, so nothing carried forward -- this
    // does).
    let mut pending_plugin_swaps: Vec<(String, String)> = Vec::new();

    // Live-found (daemon-warm-pass-scales-badly-with-registry-size):
    // sync_instruction_source_if_configured runs a real `git fetch` +
    // `git reset --hard` subprocess for any root with a configured
    // .gm/instructions/source.json -- unconditionally, every single tick,
    // for every such root. On this host's registry (73+ roots at time of
    // writing) even a handful of configured roots meant real network I/O
    // on every loop iteration, contributing to repeated daemon
    // unresponsiveness/death under load. Rate-limit to the same cadence as
    // the plugin/runner update polls (plugin_update_poll_interval) instead
    // of re-syncing every tick -- a config that rarely changes upstream
    // does not need fetching dozens of times a minute.
    let mut last_instruction_source_sync: HashMap<PathBuf, Instant> = HashMap::new();

    // Independent ticker thread: writes the heartbeat and re-checks
    // ownership authority on its own timer, never blocked by the main loop's
    // `thread::scope` worker-pool join (see spawn_heartbeat_ticker's doc
    // comment for the incident this replaces). The main loop only ever
    // POLLS `heartbeat_authority_lost()` -- a single relaxed atomic load,
    // negligible cost -- and performs the actual shutdown itself once it's
    // set, since closing browser sessions and returning from this function
    // are main-thread-owned actions the ticker must never perform directly.
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

        // Per-project threading: previously this loop dispatched every root
        // fully sequentially on one thread, so a long-running verb on
        // project A stalled every other registered project (B, C, D...)
        // behind it in the same iteration -- live-witnessed this session as
        // a single ~180s exec_js call on one project freezing the shared
        // daemon's heartbeat and starving every other project's spool
        // dispatch, which is what actually drove callers to time out and
        // spawn competing standalone watchers (see the "spool" fallback fix
        // in main.rs). Fix: run up to MAX_CONCURRENT_PROJECTS roots'
        // dispatch bodies on real OS threads simultaneously via
        // thread::scope, chunked so no more than that many run at once --
        // "threading up to 4 projects at a time, queuing the rest" per the
        // explicit design directive. Each root still processes its OWN
        // in_dir fully sequentially within its thread (one file at a time,
        // same claim-rename-dispatch-respond order as before) -- a single
        // project's own dispatches are never reordered or run concurrently
        // against each other, only DIFFERENT projects' work now overlaps.
        // ProjectPlugins entries are looked up/inserted by each thread under
        // a scoped mutable borrow of a disjoint key in the shared `projects`
        // map (each root maps to at most one thread per chunk), so this
        // never needs a Mutex around the map itself -- the borrow checker
        // enforces the disjointness via retain_mut-style partition below.
        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

        // Compile-ahead pass, sequential on the main thread, BEFORE any
        // worker thread spawns: PluginModules::get_or_compile needs &mut
        // self (it downloads+Module::from_file's a not-yet-seen plugin), but
        // dispatch_project's worker threads only ever get a shared
        // &PluginModules -- they can look a compiled Module up, never
        // compile a new one themselves (that would need a Mutex around the
        // whole engine+modules map, serializing every project on first-use
        // of any plugin, defeating the point of this refactor). So warm
        // every requested-but-not-yet-compiled plugin for every registered
        // root here first; workers then only ever hit the fast "already
        // compiled" path. A plugin whose compile genuinely fails here still
        // surfaces the real error per-dispatch inside dispatch_project (the
        // "not compiled yet -- retry shortly" / "gm plugin failed to load"
        // branches), it just isn't retried again until the NEXT full tick.
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

        // Live-found (live-witness-cross-project-concurrency-claim): the
        // previous `for root_chunk in known_roots.chunks(MAX_CONCURRENT_PROJECTS)`
        // was a SEQUENTIAL outer loop over fixed-size chunks -- 4 roots ran
        // concurrently WITHIN one chunk, but chunk 2 could not start until
        // chunk 1's thread::scope fully joined. On a registry with more than
        // MAX_CONCURRENT_PROJECTS roots (this host: 74+ registered projects
        // in practice), a single slow dispatch anywhere in chunk 1 starved
        // every root in every later chunk, not just the 3 others sharing its
        // chunk -- live-witnessed directly: registered a 74th project, fired
        // a 15-20s busy-loop exec_js against the 1st-registered project
        // (chunk 1) concurrently with a trivial health dispatch against the
        // 74th (chunk 19), and the health response took 25-31 SECONDS --
        // essentially the full duration of the unrelated chunk-1 dispatch,
        // not the near-instant response true global bounded concurrency
        // would give. Fixed: pull ALL known_roots' ProjectPlugins up front,
        // then run them through a genuine bounded-concurrency pool -- an
        // index cursor behind a Mutex, MAX_CONCURRENT_PROJECTS worker
        // threads each looping "claim the next un-dispatched root, dispatch
        // it, repeat" until the cursor is exhausted. A slow root now only
        // ever occupies ONE of the 4 worker slots; the other 3 keep pulling
        // fresh roots from the shared cursor instead of sitting idle inside
        // a chunk boundary waiting for the slow one's chunk-mates to finish.
        let all_projects: Vec<(PathBuf, ProjectPlugins)> = known_roots
            .iter()
            .map(|root| {
                let p = projects.remove(root).unwrap_or_else(|| ProjectPlugins::new(root.clone()));
                (root.clone(), p)
            })
            .collect();
        let worker_count = max_concurrent_projects.min(all_projects.len().max(1));
        // Genuine work-stealing queue, no unsafe: each (root, ProjectPlugins)
        // pair is MOVED into the shared queue, so a worker that pops one
        // holds exclusive ownership -- no aliasing question at all, unlike
        // an index-into-shared-Vec scheme. Completed pairs are pushed to
        // `done` (also behind a Mutex) as workers finish, then drained back
        // out after the scope joins. A worker with an empty queue simply
        // exits its loop rather than blocking, since the total work list is
        // fully known up front (this is one tick's fixed root set, not an
        // open-ended stream).
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
        // Keep the heartbeat ticker's own reported active_projects/
        // compiled_plugin_modules figures reasonably fresh -- it can only
        // ever read these atomics (it must never touch `projects`/
        // `plugin_modules` directly, both main-thread-owned), so the main
        // loop is responsible for publishing them after every batch.
        HEARTBEAT_PROJECT_COUNT.store(projects.len(), std::sync::atomic::Ordering::Relaxed);
        HEARTBEAT_PLUGIN_MODULE_COUNT.store(plugin_modules.modules.len(), std::sync::atomic::Ordering::Relaxed);
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            return Ok(());
        }
        // checked_sub, not bare `-`: on a freshly-started process (this loop's very first
        // iteration, right after boot) Instant::now() can be younger than PLUGIN_IDLE_EVICT_MS
        // since the process's own monotonic-clock epoch, and the bare subtraction underflows
        // -- live-reproduced as a real panic ("overflow when subtracting duration from
        // instant", std::time.rs) crashing the daemon on every boot attempt right after a
        // machine reboot (freshly-started process, evict window wider than process uptime).
        // None() means nothing is old enough to evict yet -- correct behavior when the
        // process itself hasn't lived PLUGIN_IDLE_EVICT_MS -- so fall back to Instant::now()
        // (the eviction filter below then finds zero eligible projects, same net effect as
        // properly having "no evict_before cutoff yet").
        let evict_before = Instant::now().checked_sub(Duration::from_millis(PLUGIN_IDLE_EVICT_MS)).unwrap_or_else(Instant::now);
        let to_evict: Vec<PathBuf> = projects.iter().filter(|(_, p)| p.last_active < evict_before).map(|(root, _)| root.clone()).collect();
        for root in to_evict {
            eprintln!("[agentplug daemon] evicting idle project {}", root.display());
            projects.remove(&root);
        }

        // Release the memory-heavy shared plugins once a dispatch burst has
        // gone quiet, long before the whole-process recycle below. wasm linear
        // memory only grows -- there is no shrink instruction -- so a plugin
        // that spikes during one workload pins that peak for as long as its
        // Store lives. Measured here: 350MB committed on a fresh daemon,
        // 538MB with all four plugins instantiated, 1545MB after ONE
        // full-repo codeinsight pass, and it stays at 1545MB afterward.
        // VirtualQueryEx attributes the step to a single ~1285MB contiguous
        // PAGE_READWRITE private region -- bert's own grown linear memory.
        // Dropping the Store hands those pages back; load_plugin transparently
        // re-instantiates on the next dispatch that needs the plugin (the
        // compiled Module stays cached in the Engine, so only Store+Instance
        // are rebuilt). "bert" is the only one worth releasing on this cadence:
        // it owns essentially all of the growth, while treesitter/libsql/gm
        // stay small and would just pay pointless re-instantiation churn.
        //
        // Judge any change here by PrivateMemorySize64, never WorkingSet64:
        // Windows trims a cold working set hard (a forced EmptyWorkingSet
        // dropped WorkingSet 1545.8MB -> 1.0MB while PrivateMemorySize64 held
        // at 1546.6MB), which looks exactly like accumulate-then-release even
        // when nothing has actually been freed.
        // A plugin wasm already on disk is never re-fetched by
        // ensure_plugin_installed (its dest.exists() fast path returns before
        // the version check), so without this poll a running daemon serves the
        // wasm it first downloaded forever -- no published fix ever reaches it.
        // Releasing the shared Store is what actually makes a refreshed wasm
        // take effect: the module is re-read from disk on next instantiation.
        //
        // Split the same way as the runner self-update block below (see its
        // comment for the live-witnessed starvation this pattern fixes): the
        // version check + download + sha256-verify + atomic rename never
        // touches the running process (refresh_plugin_if_stale writes to a
        // fresh path and only overwrites the on-disk .wasm/.version files),
        // so it is safe to run on every poll interval regardless of load --
        // previously this whole block, including the network check itself,
        // was gated on `!any_work`, so a daemon serving continuous
        // back-to-back project dispatch traffic (any_work true on
        // effectively every tick, same as the runner-update case) never even
        // checked for a newer gm/bert/libsql/treesitter release. Only the
        // live-swap (evicting the cached Module + releasing the shared
        // Store, which DOES touch state a concurrent dispatch could be
        // reading) stays idle-gated, queued in pending_plugin_swaps until a
        // genuinely quiet tick.
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

        // Idle-gated half of the split above: only swap in a downloaded
        // update when nothing is concurrently dispatching against the
        // plugin's cached Module/Store. Drains the whole queue on the first
        // idle tick rather than one-per-tick, since by this point every
        // entry is already downloaded+verified on disk -- the swap itself is
        // just a HashMap remove + Store drop, cheap enough to do all at once.
        if !any_work && !pending_plugin_swaps.is_empty() {
            for (plugin_name, new_version) in pending_plugin_swaps.drain(..) {
                plugin_modules.modules.remove(&plugin_name);
                agentplug_host::release_shared_plugin(&plugin_name);
                eprintln!(
                    "[agentplug daemon] refreshed plugin {plugin_name} to {new_version} -- released its Store; next call re-instantiates from the new wasm"
                );
            }
        }

        // Staging (download+verify to `<exe>.new`) is deliberately NOT gated
        // on `!any_work` -- it never touches the running process, so it's
        // safe under any load. A daemon under sustained multi-project
        // dispatch traffic can have `any_work` true on effectively every
        // tick forever (any single project's request satisfies it), which
        // previously starved this entire block -- including the staging
        // download itself -- indefinitely: a busy daemon could poll for
        // months and never even check for a new version, let alone hand off.
        // Live-witnessed this session: a staged `.new` sat unapplied through
        // multiple `bun x gm-plugkit@latest spool` reboots on a daemon that
        // never had a quiet tick. Splitting "stage" (always attempted, once
        // per interval) from "handoff" (still `!any_work`-gated below) means
        // the version check and download always keep making progress, and
        // the handoff itself still waits for a genuinely idle moment so it
        // never interrupts in-flight dispatch.
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

        // Only when genuinely idle: a self-update handoff briefly needs to
        // spawn+wait+release, which must never interrupt an in-flight
        // dispatch. On success this process's job is done -- it has
        // voluntarily released ownership to the new process, so it exits the
        // loop (and the function) here rather than looping once more and
        // re-claiming what it just handed off. `pending_self_update` persists
        // across ticks (set above, only cleared here) so a version staged
        // during a busy tick is still handed off on the FIRST idle tick that
        // follows, rather than being re-downloaded or lost.
        if !any_work {
            if let Some((staged, version)) = pending_self_update.take() {
                if attempt_self_update_handoff(&staged, &version) {
                    // Close every live browser session BEFORE exiting -- the new
                    // process takes ownership with its own empty SESSIONS
                    // registry, so any session left open here becomes an
                    // untracked orphan chrome.exe the new process can never see
                    // via session list/close, and the idle reaper can't reap
                    // what it doesn't know exists. A crash/hard exit still
                    // orphans sessions unavoidably, but this is a VOLUNTARY
                    // exit with time to clean up first -- see
                    // close_all_sessions's own doc comment for the live-hit
                    // this closes.
                    agentplug_host::close_all_sessions();
                    eprintln!("[agentplug daemon] handed off to version {version} -- exiting");
                    return Ok(());
                }
                // Handoff attempt failed (new process never confirmed ready in
                // time) -- do not silently drop the staged binary; the next
                // stage_runner_self_update() call will see installed_runner_version()
                // still behind and simply re-stage/re-verify, so falling
                // through here (pending_self_update now None) just means one
                // extra download next interval instead of a permanently lost
                // update attempt.
            }
        }

        if any_work {
            last_shared_release = Instant::now();
        } else if last_shared_release.elapsed() >= Duration::from_millis(SHARED_PLUGIN_RELEASE_IDLE_MS) {
            // Only bert was released, but treesitter and libsql grow their own
            // linear memory across an indexing pass and never gave it back --
            // wasm memory.grow has no inverse, so a Store retains its peak for
            // its whole lifetime and dropping it is the only way to reclaim.
            // `gm` is deliberately excluded despite being release-eligible: it
            // holds the orchestrator state this loop is actively serving.
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
