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
            eprintln!("[agentplug] shared daemon unavailable, falling back to a dedicated per-project process");

            let wasm = download::ensure_plugin_installed("gm", None)?;
            let engine = build_engine()?;
            let module = Module::from_file(&engine, &wasm)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, "gm", &module)?;
            run_spool_watcher_single_process(&mut project, &spool_dir)
        }
        "daemon" => daemon::run_daemon(),
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
                "agentplug-runner: unknown command '{other}'. Usage: agentplug-runner <plugin <name> [version]|spool|daemon|dispatch [plugin] <verb> [body]|version>"
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
