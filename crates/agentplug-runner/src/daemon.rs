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
const DAEMON_STALE_MS: u64 = 60_000;

fn daemon_status_path() -> PathBuf {
    install_dir().join("daemon-status.json")
}

fn daemon_lock_path() -> PathBuf {
    install_dir().join("daemon.lock")
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
    let mut plugin_modules = PluginModules::new()?;
    write_daemon_heartbeat(0, 0);

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now() - REGISTRY_POLL_INTERVAL;
    let mut last_heartbeat = Instant::now();
    let mut known_roots: Vec<PathBuf> = Vec::new();
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            write_daemon_heartbeat(projects.len(), plugin_modules.modules.len());
        }

        if last_registry_poll.elapsed() >= REGISTRY_POLL_INTERVAL {
            last_registry_poll = Instant::now();
            known_roots = read_registry();
        }

        let mut any_work = false;
        for root in &known_roots {
            let spool_dir = root.join(".gm").join("exec-spool");
            let in_dir = spool_dir.join("in");
            let out_dir = spool_dir.join("out");
            if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
                continue;
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
                }
                list
            };

            let Ok(entries) = fs::read_dir(&in_dir) else { continue };
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
                    any_work = true;
                    let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let body = fs::read_to_string(&file_path).unwrap_or_default();
                    let _ = fs::remove_file(&file_path);

                    let project = projects.entry(root.clone()).or_insert_with(|| ProjectPlugins::new(root.clone()));

                    // Load every plugin this project has requested that isn't
                    // already loaded -- gm dispatches to "gm" by default; a
                    // project opting into libsql/bert/treesitter (via
                    // .agentplug/plugins.txt) gets those instantiated too, so
                    // gm.wasm's own host_plugin_call/host_vec_embed finds them.
                    for plugin_name in &requested_plugins {
                        if project.is_loaded(plugin_name) {
                            continue;
                        }
                        if let Err(e) = plugin_modules.get_or_compile(plugin_name) {
                            eprintln!("[agentplug daemon] failed to compile/install plugin {plugin_name}: {e:#}");
                            continue;
                        }
                        let module = plugin_modules.modules.get(plugin_name).unwrap();
                        if let Err(e) = project.load_plugin(&plugin_modules.engine, plugin_name, module) {
                            eprintln!("[agentplug daemon] failed to instantiate plugin {plugin_name} for {}: {e:#}", root.display());
                        }
                    }

                    // The "gm" plugin is the dispatch entrypoint for the
                    // existing spool ABI (in/<verb>/<N>.txt) -- other plugins
                    // are only reachable via gm.wasm's own host_plugin_call,
                    // never directly from the spool surface. This keeps the
                    // spool contract byte-identical to today's gm-runner.
                    let result = project.dispatch("gm", &verb, &body);
                    let out_name = format!("{verb}-{task}.json");
                    let out_body = match result {
                        Ok(s) if !s.is_empty() => s,
                        Ok(_) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
                        Err(e) => serde_json::json!({"ok": false, "error": format!("{e:#}"), "verb": verb}).to_string(),
                    };
                    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
                    if fs::write(&tmp, &out_body).is_ok() {
                        let _ = fs::rename(&tmp, out_dir.join(&out_name));
                    }
                }
            }

            // Generic plugin+verb dispatch surface, separate from the gm
            // spool above -- exists so a host OUTSIDE agentplug's own wasm
            // graph (e.g. gm-plugkit's JS wrapper, hosting plugkit.wasm
            // directly via Node/Bun's own WebAssembly.instantiate, not via
            // agentplug-runner) can still reach a PERSISTENT plugin instance
            // for stateful plugins (libsql: an `open` in one call must be
            // visible to a later `exec`/`query` call, which a fresh
            // one-shot `dispatch` subprocess per call can never provide --
            // each subprocess gets its own empty in-memory DBS map). Layout:
            // .agentplug/plugin-dispatch/in/<plugin>/<verb>/<N>.txt ->
            // .agentplug/plugin-dispatch/out/<plugin>-<verb>-<N>.json,
            // deliberately mirroring the gm spool's own in/out shape so
            // agentplug-runner's `dispatch` subcommand (see main.rs) can
            // poll it the same way the gm skill polls .gm/exec-spool/out/.
            let pd_dir = root.join(".agentplug").join("plugin-dispatch");
            let pd_in = pd_dir.join("in");
            let pd_out = pd_dir.join("out");
            if fs::create_dir_all(&pd_in).is_err() || fs::create_dir_all(&pd_out).is_err() {
                continue;
            }
            let Ok(plugin_dirs) = fs::read_dir(&pd_in) else { continue };
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
                        any_work = true;
                        let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        let body = fs::read_to_string(&file_path).unwrap_or_default();
                        let _ = fs::remove_file(&file_path);

                        let project = projects.entry(root.clone()).or_insert_with(|| ProjectPlugins::new(root.clone()));
                        if !project.is_loaded(&plugin_name) {
                            if let Err(e) = plugin_modules.get_or_compile(&plugin_name) {
                                let out_name = format!("{plugin_name}-{verb}-{task}.json");
                                let out_body = serde_json::json!({"ok": false, "error": format!("plugin compile/install failed: {e:#}")}).to_string();
                                let _ = fs::write(pd_out.join(out_name), out_body);
                                continue;
                            }
                            let module = plugin_modules.modules.get(&plugin_name).unwrap();
                            if let Err(e) = project.load_plugin(&plugin_modules.engine, &plugin_name, module) {
                                let out_name = format!("{plugin_name}-{verb}-{task}.json");
                                let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                                let _ = fs::write(pd_out.join(out_name), out_body);
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
                        let tmp = pd_out.join(format!("{out_name}.tmp.{}", std::process::id()));
                        if fs::write(&tmp, &out_body).is_ok() {
                            let _ = fs::rename(&tmp, pd_out.join(&out_name));
                        }
                    }
                }
            }
        }

        let evict_before = Instant::now() - Duration::from_millis(PLUGIN_IDLE_EVICT_MS);
        let to_evict: Vec<PathBuf> = projects.iter().filter(|(_, p)| p.last_active < evict_before).map(|(root, _)| root.clone()).collect();
        for root in to_evict {
            eprintln!("[agentplug daemon] evicting idle project {}", root.display());
            projects.remove(&root);
        }

        if !any_work {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
