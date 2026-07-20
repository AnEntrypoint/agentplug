use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wasmtime::{Engine, Module};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, ProjectPlugins, PLUGIN_IDLE_EVICT_MS};

use crate::download::ensure_plugin_installed;

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

const REGISTRY_POLL_INTERVAL: Duration = Duration::from_secs(5);
// 2x the daemon's own HEARTBEAT_INTERVAL (10s) plus slack, not the looser
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
    // correct owner file) is never mistaken for dead.
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
    if heartbeat_fresh {
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

fn is_daemon_fresh() -> bool {
    let Ok(raw) = fs::read_to_string(daemon_status_path()) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    let Some(ts) = v.get("ts").and_then(|t| t.as_u64()) else { return false };
    now_ms().saturating_sub(ts) < DAEMON_STALE_MS
}

fn spawn_detached_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon");
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
                    match project.dispatch("gm", &verb, &body) {
                        Ok(s) if !s.is_empty() => s,
                        Ok(_) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
                        Err(e) => serde_json::json!({"ok": false, "error": format!("{e:#}"), "verb": verb}).to_string(),
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

    let mut plugin_modules = PluginModules::new()?;
    write_daemon_heartbeat(0, 0);

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now() - REGISTRY_POLL_INTERVAL;
    let mut last_heartbeat = Instant::now();
    let mut known_roots: Vec<PathBuf> = Vec::new();
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

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

    const PLUGIN_UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(600);
    let mut last_plugin_update_poll = Instant::now();

    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
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

        if last_registry_poll.elapsed() >= REGISTRY_POLL_INTERVAL {
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
        if !any_work && last_plugin_update_poll.elapsed() >= PLUGIN_UPDATE_POLL_INTERVAL {
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
