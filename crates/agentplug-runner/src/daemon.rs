use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wasmtime::{Engine, Module};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, ProjectPlugins, PLUGIN_IDLE_EVICT_MS};

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
            })
    }
    fn registry_poll_interval(&self) -> Duration { Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5)) }
    fn heartbeat_interval(&self) -> Duration { Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10)) }
    fn plugin_update_poll_interval(&self) -> Duration { Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600)) }
    fn runner_update_poll_interval(&self) -> Duration { Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(600)) }
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

                // Load every plugin this project has requested that isn't
                // already loaded -- gm dispatches to "gm" by default; a
                // project opting into libsql/bert/treesitter (via
                // .agentplug/plugins.txt) gets those instantiated too, so
                // gm.wasm's own host_plugin_call/host_vec_embed finds them.
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

                // The "gm" plugin is the dispatch entrypoint for the
                // existing spool ABI -- fail loud with the real reason
                // instead of a dispatch that was always going to fail if
                // "gm" itself never loaded (network hiccup, plugin not yet
                // published for this platform).
                let out_name = format!("{verb}-{task}.json");
                let out_body = if !project.is_loaded("gm") {
                    serde_json::json!({"ok": false, "error": "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)", "verb": verb}).to_string()
                } else {
                    // A panic inside project.dispatch (a wasmtime trap that
                    // escapes as a Rust panic rather than surfacing through
                    // the Result, or a poisoned-Mutex unwrap somewhere in the
                    // call chain) previously unwound straight through this
                    // function with no catch -- the request file was ALREADY
                    // claimed+deleted (rename above, then remove_file) before
                    // this point, so the panic silently ate the request:
                    // no response ever written, no error surfaced, the
                    // caller left polling forever against a file that will
                    // never appear. Live-hit this session with a jit-hook
                    // gate (fsm-framework-jit-hook-concreting) whose
                    // host_exec_js call triggered exactly this. catch_unwind
                    // converts any panic into a real error response instead,
                    // so the caller gets a definitive (if unhappy) answer
                    // rather than an unbounded silent hang.
                    let dispatch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        project.dispatch("gm", &verb, &body)
                    }));
                    match dispatch_result {
                        Ok(Ok(s)) if !s.is_empty() => s,
                        Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
                        Ok(Err(e)) => serde_json::json!({"ok": false, "error": format!("{e:#}"), "verb": verb}).to_string(),
                        Err(panic_payload) => {
                            let msg = panic_payload.downcast_ref::<&str>().map(|s| s.to_string())
                                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "panic with non-string payload".to_string());
                            eprintln!("[agentplug daemon] verb {verb} PANICKED for {}: {msg}", root.display());
                            serde_json::json!({"ok": false, "error": format!("dispatch panicked: {msg}"), "verb": verb}).to_string()
                        }
                    }
                };
                let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
                if fs::write(&tmp, &out_body).is_ok() {
                    let _ = fs::rename(&tmp, out_dir.join(&out_name));
                    let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
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

                let result = project.dispatch(&plugin_name, &verb, &body);
                let out_name = format!("{plugin_name}-{verb}-{task}.json");
                let out_body = match result {
                    Ok(s) if !s.is_empty() => s,
                    Ok(_) => serde_json::json!({"ok": false, "error": "empty dispatch result"}).to_string(),
                    Err(e) => serde_json::json!({"ok": false, "error": format!("{e:#}")}).to_string(),
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

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now() - registry_poll_interval;
    let mut last_heartbeat = Instant::now();
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

    loop {
        if last_heartbeat.elapsed() >= heartbeat_interval {
            last_heartbeat = Instant::now();
            // Closes the residual TOCTOU window the startup check above
            // can't fully cover (two processes racing the exact same
            // microsecond-scale check-then-write): re-check every heartbeat
            // tick too, so a true double-spawn self-corrects within one
            // HEARTBEAT_INTERVAL instead of running both daemons forever.
            let lost_race = !holds_heartbeat_authority();
            if lost_race {
                eprintln!("[agentplug daemon] another daemon claimed heartbeat authority -- exiting");
                return Ok(());
            }
            write_daemon_heartbeat(projects.len(), plugin_modules.modules.len());
        }

        // The authority re-check above only runs inside the heartbeat branch,
        // so a daemon stuck in one long synchronous wasm call (a full index
        // pass, a batch of ~3s bert embeds) blows past HEARTBEAT_INTERVAL, a
        // newer daemon claims authority meanwhile, and the busy one keeps
        // serving nobody until it happens to return. Live-hit: pid 18820 held
        // no heartbeat (pid 22420 did) yet burned a full core -- CPU 158.1s to
        // 170.0s across a 12s sample -- and 2,827MB, versus the live daemon's
        // 542MB. Check before doing work too, so an orphan exits at the next
        // loop top rather than only at the next heartbeat it may never reach.
        if !holds_heartbeat_authority() {
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting before serving further work");
            return Ok(());
        }

        if last_registry_poll.elapsed() >= registry_poll_interval {
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
        const MAX_CONCURRENT_PROJECTS: usize = 4;

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
            if let Err(e) = sync_instruction_source_if_configured(root) {
                eprintln!("[agentplug daemon] instruction source-repo sync failed for {}: {e:#}", root.display());
            }
        }
        for plugin_name in ["gm", "libsql", "bert", "treesitter"] {
            if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
                eprintln!("[agentplug daemon] failed to compile/install default plugin {plugin_name}: {e:#}");
            }
        }

        let mut any_work = false;
        for root_chunk in known_roots.chunks(MAX_CONCURRENT_PROJECTS) {
            // Pull each chunked root's ProjectPlugins out of the shared map
            // (inserting a fresh one if new) so each thread below gets an
            // exclusively-owned &mut for the duration of this chunk -- no
            // aliasing, no lock contention between the up-to-4 threads.
            let mut chunk_projects: Vec<(PathBuf, ProjectPlugins)> = root_chunk
                .iter()
                .map(|root| {
                    let p = projects.remove(root).unwrap_or_else(|| ProjectPlugins::new(root.clone()));
                    (root.clone(), p)
                })
                .collect();
            // thread::scope's own contract: every scope.spawn'd closure's
            // borrow of chunk_projects's elements must be joined (guaranteed
            // by scope() itself not returning until all handles finish)
            // BEFORE chunk_projects can be touched again outside the scope
            // -- so the did-work flags come back OUT of the scope via the
            // handles' return values (owned bools, not borrows), and
            // chunk_projects itself (still holding every ProjectPlugins,
            // mutated in place by its thread) is read again only after
            // scope() has returned.
            let plugin_modules_ref: &PluginModules = &plugin_modules;
            let did_work_flags: Vec<bool> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk_projects
                    .iter_mut()
                    .map(|(root, project)| {
                        let root: &Path = root.as_path();
                        scope.spawn(move || dispatch_project(root, project, plugin_modules_ref))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
            });
            for ((root, project), did_work) in chunk_projects.into_iter().zip(did_work_flags) {
                any_work = any_work || did_work;
                projects.insert(root, project);
            }
        }
        let evict_before = Instant::now() - Duration::from_millis(PLUGIN_IDLE_EVICT_MS);
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
        if !any_work && last_plugin_update_poll.elapsed() >= plugin_update_poll_interval {
            last_plugin_update_poll = Instant::now();
            for plugin_name in plugin_modules.modules.keys().cloned().collect::<Vec<_>>() {
                match crate::download::refresh_plugin_if_stale(&plugin_name) {
                    Ok(Some(new_version)) => {
                        plugin_modules.modules.remove(&plugin_name);
                        agentplug_host::release_shared_plugin(&plugin_name);
                        eprintln!(
                            "[agentplug daemon] refreshed plugin {plugin_name} to {new_version} -- released its Store; next call re-instantiates from the new wasm"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[agentplug daemon] plugin update check for {plugin_name} failed: {e}"),
                }
            }
        }

        // Only when genuinely idle, same reasoning as the plugin-update poll
        // above: a self-update handoff briefly needs to spawn+wait+release,
        // which must never interrupt an in-flight dispatch. On success this
        // process's job is done -- it has voluntarily released ownership to
        // the new process, so it exits the loop (and the function) here
        // rather than looping once more and re-claiming what it just handed
        // off.
        if !any_work && last_runner_update_poll.elapsed() >= runner_update_poll_interval {
            last_runner_update_poll = Instant::now();
            match crate::download::stage_runner_self_update() {
                Ok(Some((staged, version))) => {
                    eprintln!("[agentplug daemon] staged self-update to {version} at {}", staged.display());
                    if attempt_self_update_handoff(&staged, &version) {
                        eprintln!("[agentplug daemon] handed off to version {version} -- exiting");
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("[agentplug daemon] runner self-update check failed: {e}"),
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
