mod daemon;
mod download;

use std::path::PathBuf;

use agentplug_host::{build_engine, ProjectPlugins};
use wasmtime::Module;

/// Same command surface gm-runner's own main.rs exposes (bootstrap/spool/
/// dispatch/progress/version) plus `plugin <name> [version]` -- a project's
/// gm-plugkit installer or cli.js can spawn `agentplug-runner spool` exactly
/// where it previously spawned `gm-runner spool`, zero ABI change on the
/// spool-dir side.
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");

    match cmd {
        "plugin" => {
            let name = args.get(2).cloned().unwrap_or_default();
            if name.is_empty() {
                eprintln!("usage: agentplug-runner plugin <name> [version]");
                std::process::exit(1);
            }
            let version = args.get(3).cloned();
            let dest = download::ensure_plugin_installed(&name, version.as_deref())?;
            println!("{name}.wasm installed at {}", dest.display());
            Ok(())
        }
        "spool" => {
            let cwd = std::env::var("CLAUDE_PROJECT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::current_dir().expect("cwd unavailable"));
            let spool_dir = cwd.join(".gm").join("exec-spool");
            std::fs::create_dir_all(&spool_dir)?;

            // No eager gm.wasm download here -- that used to block this
            // entire invocation for minutes on a cold cache (gm.wasm is a
            // real ~137MB artifact), violating the gm skill's own
            // documented "spool is fire-and-forget, does not wait"
            // contract, live-witnessed this session as a 20s+ hang on a
            // command that should return near-instantly. The daemon's own
            // loop (daemon.rs::PluginModules::get_or_compile) already
            // downloads "gm" lazily on first real dispatch need for a
            // registered project, the exact same lazy pattern already
            // proven correct for libsql/bert/treesitter -- register and
            // hand off, don't block on it here too.
            daemon::register_project(&cwd)?;
            if daemon::ensure_daemon_running()? {
                eprintln!(
                    "[agentplug] registered {} with the shared system-wide daemon -- no dedicated per-project process spawned",
                    cwd.display()
                );
                return Ok(());
            }
            // ensure_daemon_running()'s bounded wait (~6s) can time out while
            // the actual winning daemon is still mid-PluginModules::new()
            // (wasm engine build + compile of gm/bert/libsql/treesitter,
            // witnessed this session taking well over 6s under load) and
            // hasn't written its first heartbeat yet. Previously every OTHER
            // spool invocation that timed out fell straight through to a
            // standalone long-lived watcher that NEVER calls
            // claim_ownership()/run_daemon() at all -- not a race the atomic
            // guard was ever positioned to prevent, since it's a separate
            // process that never contests the lock. Two (or more) such
            // standalone watchers then coexisted indefinitely, each serving
            // its own project -- the "multiple agentplug-runner processes"
            // symptom, live-witnessed this session. Fix: attempt to become
            // the ONE shared daemon here too via run_daemon() before ever
            // falling back to a private one-shot instance. If this process
            // wins the atomic claim, it becomes the real long-lived daemon
            // (serving this project and any other that finds it) and never
            // returns. If it loses (the real winner finished compiling and
            // claimed first while we were building our own engine or
            // waiting), run_daemon() returns Ok(()) immediately with zero
            // plugin state touched -- exactly like the earlier bounded-wait
            // loss -- so retry the shared-daemon path once more now that the
            // real winner should be visible. This does NOT fully eliminate a
            // standalone watcher spawning below (run_spool_watcher_single_process
            // is still a long-lived loop, not a one-shot) -- it narrows the
            // window in which one can be spawned at all: a standalone watcher
            // now only starts if the shared daemon is STILL unclaimed after
            // this process itself tried and failed to become it, meaning two
            // consecutive compile-plus-wait cycles both missed the real
            // daemon, a materially rarer case than the original single
            // bounded wait.
            eprintln!("[agentplug] shared daemon not yet visible, attempting to become it before falling back");
            daemon::run_daemon()?;

            if daemon::ensure_daemon_running()? {
                eprintln!(
                    "[agentplug] registered {} with the shared system-wide daemon (converged after retry) -- no dedicated per-project process spawned",
                    cwd.display()
                );
                return Ok(());
            }

            eprintln!("[agentplug] shared daemon still unavailable after retry -- falling back to a standalone watcher for this project");
            let wasm = download::ensure_plugin_installed("gm", None)?;
            let engine = build_engine()?;
            let module = Module::from_file(&engine, &wasm)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, "gm", &module)?;
            run_spool_watcher_single_process(&mut project, &spool_dir)
        }
        "daemon" => daemon::run_daemon(),
        "takeover" => {
            let version = args.get(2).cloned().unwrap_or_default();
            if version.is_empty() {
                eprintln!("usage: agentplug-runner takeover <version>");
                std::process::exit(1);
            }
            daemon::run_takeover(&version)
        }
        "dispatch" => {
            let plugin = args.get(2).cloned().unwrap_or_else(|| "gm".to_string());
            let verb = args.get(3).cloned().unwrap_or_default();
            let body = args.get(4).cloned().unwrap_or_else(|| "{}".to_string());
            let cwd = std::env::current_dir()?;

            // Route through the shared daemon when reachable -- a plain
            // one-shot instantiate-per-call (the fallback below) is fine
            // for stateless plugins (bert:embed, treesitter:parse) but
            // fundamentally wrong for a stateful one like libsql, where an
            // "open" in one process must still be visible to a later
            // "exec"/"query": each standalone subprocess gets its own
            // empty in-memory connection table, so open-then-query across
            // two separate `dispatch` invocations always fails
            // "no dbs open" even though the plugin itself is correct. The
            // daemon keeps one persistent ProjectPlugins per (root, plugin)
            // across calls, which is the only place this can genuinely work.
            if let Some(out) = daemon::try_dispatch_via_daemon(&cwd, &plugin, &verb, &body) {
                println!("{out}");
                return Ok(());
            }

            let wasm = download::ensure_plugin_installed(&plugin, None)?;
            let engine = build_engine()?;
            let module = Module::from_file(&engine, &wasm)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, &plugin, &module)?;
            let out = project.dispatch(&plugin, &verb, &body)?;
            println!("{out}");
            Ok(())
        }
        "--version" | "version" => {
            println!("agentplug-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => {
            eprintln!(
                "agentplug-runner: unknown command '{other}'. Usage: agentplug-runner <plugin <name> [version]|spool|daemon|takeover <version>|dispatch [plugin] <verb> [body]|version>"
            );
            std::process::exit(1);
        }
    }
}

/// Fallback path when the shared daemon is unavailable (lock contention
/// timeout) -- a dedicated per-project process serving just the "gm" plugin,
/// same spool polling loop shape as gm-runner's own run_spool_watcher.
fn run_spool_watcher_single_process(project: &mut ProjectPlugins, spool_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::fs;
    use std::time::Duration;

    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    fs::create_dir_all(&in_dir)?;
    fs::create_dir_all(&out_dir)?;
    let status_path = spool_dir.join(".status.json");

    loop {
        let _ = fs::write(
            &status_path,
            serde_json::json!({"pid": std::process::id(), "ts": agentplug_host::now_ms(), "runtime": "agentplug-runner-standalone"}).to_string(),
        );

        let mut work_done = false;
        if let Ok(verb_dirs) = fs::read_dir(&in_dir) {
            for verb_entry in verb_dirs.flatten() {
                if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let verb = verb_entry.file_name().to_string_lossy().into_owned();
                let verb_dir = verb_entry.path();
                let Ok(files) = fs::read_dir(&verb_dir) else { continue };
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                        continue;
                    }
                    let Ok(body) = fs::read_to_string(&path) else { continue };
                    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let result = project
                        .dispatch("gm", &verb, &body)
                        .unwrap_or_else(|e| serde_json::json!({"ok": false, "verb": verb, "error": e.to_string()}).to_string());
                    let out_path = out_dir.join(format!("{verb}-{stem}.json"));
                    fs::write(&out_path, result)?;
                    let _ = fs::remove_file(&path);
                    work_done = true;
                }
            }
        }
        if !work_done {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}
