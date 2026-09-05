mod daemon;
mod download;

use std::path::PathBuf;

use agentplug_host::{advance_plugin_fiber, build_engine, get_active_provider, ProjectPlugins};
use wasmtime::Module;

#[cfg(windows)]
fn suppress_crash_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
    };
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
}

#[cfg(not(windows))]
fn suppress_crash_dialogs() {}

fn reconcile_plugin_manifest(
    project: &mut ProjectPlugins,
    engine: &wasmtime::Engine,
    desired: &[(&str, Option<&str>)],
) -> anyhow::Result<Vec<String>> {
    let mut reloaded = Vec::new();
    for (name, explicit_version) in desired {
        let wasm = match download::ensure_plugin_installed(name, *explicit_version) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let bytes = match std::fs::read(&wasm) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let content_hash = download::sha256_hex(&bytes);
        let module = match agentplug_host::load_module_file_backed(engine, &wasm, name, &content_hash) {
            Ok(m) => m,
            Err(_) => {
                advance_plugin_fiber(name, false, None);
                continue;
            }
        };
        let load_result = project.load_plugin(engine, name, &module, &content_hash);
        advance_plugin_fiber(name, load_result.is_ok(), Some(&content_hash));
        if load_result.is_ok() {
            if let Some(active) = get_active_provider(name) {
                if active != content_hash {
                    eprintln!(
                        "reconcile_plugin_manifest: {name} loaded {content_hash} but broker's active provider still reports {active} (multi-slot pool, expected under partial fill)"
                    );
                }
            }
            reloaded.push(name.to_string());
        }
    }
    Ok(reloaded)
}

fn main() -> anyhow::Result<()> {
    suppress_crash_dialogs();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        eprintln!("[agentplug daemon] PANIC pid={} at {loc}: {info}", std::process::id());
        agentplug_host::close_all_sessions();
        default_hook(info);
    }));

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

            daemon::register_project(&cwd)?;
            if daemon::ensure_daemon_running()? {
                eprintln!(
                    "[agentplug] registered {} with the shared system-wide daemon -- no dedicated per-project process spawned",
                    cwd.display()
                );
                return Ok(());
            }
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
            let content_hash = download::sha256_hex(&std::fs::read(&wasm)?);
            let engine = build_engine()?;
            let module = agentplug_host::load_module_file_backed(&engine, &wasm, "gm", &content_hash)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, "gm", &module, &content_hash)?;
            run_spool_watcher_single_process(&mut project, &spool_dir)
        }
        "daemon" => daemon::run_daemon(),
        "sweep-spool" => {
            let root = args.get(2).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().expect("cwd unavailable"));
            daemon::sweep_orphaned_claims(&root);
            daemon::sweep_unconsumable_spool_files(&root);
            println!("swept orphaned claims and unconsumable spool files under {}", root.display());
            Ok(())
        }
        "reap-orphans" => {
            let roots = daemon::read_registry();
            agentplug_host::reap_idle_sessions_and_os_orphans_across_every_known_project_root(&roots);
            println!("reaped idle sessions and orphaned chrome processes across {} registered project roots (plus the process-global headless-orphan sweep)", roots.len());
            Ok(())
        }
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

            if let Some(out) = daemon::try_dispatch_via_daemon(&cwd, &plugin, &verb, &body) {
                println!("{out}");
                return Ok(());
            }

            let wasm = download::ensure_plugin_installed(&plugin, None)?;
            let content_hash = download::sha256_hex(&std::fs::read(&wasm)?);
            let engine = build_engine()?;
            let module = agentplug_host::load_module_file_backed(&engine, &wasm, &plugin, &content_hash)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, &plugin, &module, &content_hash)?;
            let siblings: Vec<(&str, Option<&str>)> = ["libsql", "bert", "treesitter"]
                .iter()
                .filter(|side| **side != plugin)
                .map(|side| (*side, None))
                .collect();
            let _ = reconcile_plugin_manifest(&mut project, &engine, &siblings)?;
            let out = project.dispatch(&plugin, &verb, &body)?;
            println!("{out}");
            Ok(())
        }
        "--version" | "version" => {
            println!("agentplug-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "selfcheck-registry" => selfcheck_registry(),
        "selfcheck-inflight" => selfcheck_inflight_cleanup(),
        other => {
            eprintln!(
                "agentplug-runner: unknown command '{other}'. Usage: agentplug-runner <plugin <name> [version]|spool|daemon|takeover <version>|dispatch [plugin] <verb> [body]|reap-orphans|sweep-spool [root]|selfcheck-registry|selfcheck-inflight|version>"
            );
            std::process::exit(1);
        }
    }
}

const SELFCHECK_SUCCESS_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "plugkit_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "plugkit_free") (param i32 i32))
  (func (export "plugin_call") (param i32 i32 i32 i32) (result i64) (i64.const 8589936640))
  (data (i32.const 2048) "ok")
)"#;

fn selfcheck_registry() -> anyhow::Result<()> {
    use agentplug_host::{note_shared_plugin_bytes_current, request_shared_store_swap, shared_plugin_slot_content_hashes, shared_plugin_swap_pending_hashes};

    let engine = build_engine()?;
    let module = Module::new(&engine, SELFCHECK_SUCCESS_WAT)?;
    let root = std::env::temp_dir().join(format!("agentplug-selfcheck-registry-{}", std::process::id()));
    let mut project = ProjectPlugins::new(root.clone());
    project.load_plugin(&engine, "gm", &module, "hash-a")?;
    let out = project.dispatch("gm", "probe", "{}")?;
    assert_eq!(out, "ok", "fresh slot must serve a real dispatch through the compiled module");
    println!("[selfcheck-registry] fresh gm slot dispatched and returned {out:?}");

    let (evicted_now, deferred) = request_shared_store_swap("gm", "hash-a");
    println!("[selfcheck-registry] swap request against idle slot: evicted_now={evicted_now} deferred={deferred}");
    assert_eq!((evicted_now, deferred), (1, 0), "an idle slot holding the old hash must be evicted immediately, nothing deferred");
    assert!(shared_plugin_slot_content_hashes("gm").iter().all(|h| h.is_none()), "evicted slot must show no content hash");

    project.load_plugin(&engine, "gm", &module, "hash-b")?;
    let out2 = project.dispatch("gm", "probe", "{}")?;
    assert_eq!(out2, "ok", "reinstantiated slot on the new hash must still serve real dispatches");
    println!("[selfcheck-registry] slot reinstantiated on hash-b and served a second real dispatch: {out2:?}");

    note_shared_plugin_bytes_current("gm", "hash-b");
    assert!(shared_plugin_swap_pending_hashes("gm").is_empty(), "marking hash-b current must leave no pending swap hashes");
    println!("[selfcheck-registry] all invariants witnessed live through real wasmtime dispatch: PASS");
    Ok(())
}

fn selfcheck_inflight_cleanup() -> anyhow::Result<()> {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let root = std::env::temp_dir().join(format!("agentplug-selfcheck-inflight-{}-{}", std::process::id(), agentplug_host::now_ms()));
    let spool_dir = root.join(".gm").join("exec-spool");
    let out_dir = spool_dir.join("out");
    fs::create_dir_all(&out_dir)?;

    let project = ProjectPlugins::new(root.clone());
    let handle = project.dispatch_handle();
    let key: daemon::InFlightKey = (root.clone(), "verbX".to_string(), "taskY".to_string());
    daemon::in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), daemon::InFlightHandle { detach: Arc::new(AtomicBool::new(false)) });

    daemon::run_gm_dispatch_to_file(&root, &handle, "verbX", "taskY", "{}", &out_dir);

    let entry_remains = daemon::in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_some();
    let out_written = out_dir.join("verbX-taskY.json").exists();
    println!("[selfcheck-inflight] entry_remains={entry_remains} out_written={out_written}");
    assert!(!entry_remains, "a completed dispatch must clear its own in-flight entry so the handoff/idle gates stop counting it");
    assert!(out_written, "the out file must still be written even when the dispatch itself errors (no registered plugin)");

    let _ = fs::remove_dir_all(&root);
    println!("[selfcheck-inflight] witnessed live against the real daemon dispatch path: PASS");
    Ok(())
}

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
