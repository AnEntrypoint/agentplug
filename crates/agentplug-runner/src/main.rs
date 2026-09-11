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
        let module = match Module::from_file(engine, &wasm) {
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
            if let Some(owner_pid) = daemon::shared_daemon_owner_that_would_refuse_this_process() {
                eprintln!(
                    "[agentplug] shared daemon pid {owner_pid} owns the daemon lock with a fresh heartbeat -- {} stays registered with it, no competing daemon or standalone watcher started",
                    cwd.display()
                );
                return Ok(());
            }

            if let Some(remaining_ms) = daemon::daemon_spawn_backoff_remaining_ms_if_active() {
                eprintln!(
                    "[agentplug] a recent daemon start already lost the ownership claim and the backoff has {remaining_ms}ms left -- {} stays registered, no further start attempt and no standalone watcher this call",
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

            if let Some(owner_pid) = daemon::live_foreign_daemon_owner_pid() {
                eprintln!(
                    "[agentplug] daemon pid {owner_pid} is alive and owns the shared lock -- {} stays registered with it, no standalone watcher started (a second sweeper on one spool double-claims its requests)",
                    cwd.display()
                );
                return Ok(());
            }

            if let Some(sweeper_pid) = daemon::live_foreign_spool_sweeper(&spool_dir) {
                eprintln!(
                    "[agentplug] pid {sweeper_pid} is already sweeping {} with a live heartbeat -- {} stays registered with it, no second standalone watcher started (two sweepers on one spool orphan each other's in-flight claims)",
                    spool_dir.display(),
                    cwd.display()
                );
                return Ok(());
            }

            eprintln!("[agentplug] shared daemon still unavailable after retry -- falling back to a standalone watcher for this project");
            let wasm = download::ensure_plugin_installed("gm", None)?;
            let content_hash = download::sha256_hex(&std::fs::read(&wasm)?);
            let engine = build_engine()?;
            let module = Module::from_file(&engine, &wasm)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, "gm", &module, &content_hash)?;
            let _ = reconcile_plugin_manifest(&mut project, &engine, &[("libsql", None), ("bert", None), ("treesitter", None)]);
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
            let module = Module::from_file(&engine, &wasm)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, &plugin, &module, &content_hash)?;
            let siblings: Vec<(&str, Option<&str>)> = ["libsql", "bert", "treesitter"]
                .iter()
                .filter(|side| **side != plugin)
                .map(|side| (*side, None))
                .collect();
            let _ = reconcile_plugin_manifest(&mut project, &engine, &siblings)?;
            let out = project.dispatch(&plugin, &verb, &body)?;
            let out = daemon::patch_update_available_from_escalation(&plugin, &verb, out);
            println!("{out}");
            Ok(())
        }
        "--version" | "version" => {
            println!("agentplug-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "selfcheck-registry" => selfcheck_registry(),
        "selfcheck-inflight" => selfcheck_inflight_cleanup(),
        "selfcheck-pool-fairness" => selfcheck_pool_fairness(),
        "selfcheck-spool-claim" => selfcheck_spool_claim(),
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

    let gm_slot_count = shared_plugin_slot_content_hashes("gm").len();
    let (evicted_now, deferred) = request_shared_store_swap("gm", "hash-a");
    println!("[selfcheck-registry] swap request against {gm_slot_count} idle slot(s): evicted_now={evicted_now} deferred={deferred}");
    assert_eq!((evicted_now, deferred), (gm_slot_count, 0), "every idle slot holding the old hash must be evicted immediately, nothing deferred");
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

fn selfcheck_pool_fairness() -> anyhow::Result<()> {
    use agentplug_host::{cost_class_for_dispatch, cost_class_for_verb, DispatchCostClass, SharedPluginPool};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    assert_eq!(cost_class_for_verb("code_index"), DispatchCostClass::Heavy, "code_index must classify as heavy");
    assert_eq!(cost_class_for_verb("recall"), DispatchCostClass::Heavy, "recall must classify as heavy");
    assert_eq!(cost_class_for_verb("codesearch"), DispatchCostClass::Cheap, "the verb-only compatibility classifier must stay cheap");
    assert_eq!(cost_class_for_dispatch("codesearch", r#"{"mode":"literal"}"#), DispatchCostClass::Cheap, "literal codesearch must preserve a slot for responsive tool work");
    assert_eq!(cost_class_for_dispatch("codesearch", r#"{"mode":"regex"}"#), DispatchCostClass::Cheap, "regex codesearch must preserve a slot for responsive tool work");
    assert_eq!(cost_class_for_dispatch("codesearch", r#"{"query":"semantic search"}"#), DispatchCostClass::Heavy, "default codesearch can rebuild and embed, so it must not consume the responsive slot");
    assert_eq!(cost_class_for_verb("instruction"), DispatchCostClass::Cheap, "instruction must classify as cheap");
    println!("[selfcheck-pool-fairness] verb classification: code_index/recall heavy, codesearch/instruction cheap");

    const POOL_SIZE: usize = 4;
    let pool = Arc::new(SharedPluginPool::new("gm", POOL_SIZE));
    let heavy_slots_expected = POOL_SIZE - 1;

    let (parked_tx, parked_rx) = mpsc::channel::<usize>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let mut heavy_threads = Vec::new();
    for n in 0..heavy_slots_expected {
        let pool = pool.clone();
        let parked_tx = parked_tx.clone();
        let release_rx = release_rx.clone();
        heavy_threads.push(std::thread::spawn(move || {
            let _admission = SharedPluginPool::admit(&pool, DispatchCostClass::Heavy);
            let (_guard, _waited) = pool.acquire_within_for_class(SharedPluginPool::ACQUIRE_TIMEOUT_MS, DispatchCostClass::Heavy);
            let _ = parked_tx.send(n);
            let _ = release_rx.lock().unwrap().recv();
        }));
    }
    for _ in 0..heavy_slots_expected {
        let parked = parked_rx.recv_timeout(Duration::from_secs(10)).map_err(|e| anyhow::anyhow!("a heavy dispatch never acquired its slot: {e}"))?;
        println!("[selfcheck-pool-fairness] heavy dispatch {parked} parked holding a real slot guard");
    }

    let extra_heavy_pool = pool.clone();
    let (extra_admitted_tx, extra_admitted_rx) = mpsc::channel::<()>();
    let extra_heavy = std::thread::spawn(move || {
        let _admission = SharedPluginPool::admit(&extra_heavy_pool, DispatchCostClass::Heavy);
        let _ = extra_admitted_tx.send(());
    });
    assert!(
        extra_admitted_rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "a {}th heavy dispatch must NOT be admitted into a {POOL_SIZE}-slot pool -- one slot stays reserved for cheap verbs",
        heavy_slots_expected + 1
    );
    println!("[selfcheck-pool-fairness] a further heavy dispatch is held at admission, so it cannot take the reserved slot");

    let cheap_start = Instant::now();
    let (cheap_guard, cheap_waited_ms) = pool.acquire_within_for_class(SharedPluginPool::ACQUIRE_TIMEOUT_MS, DispatchCostClass::Cheap);
    let cheap_elapsed = cheap_start.elapsed();
    assert!(
        cheap_elapsed < Duration::from_millis(500),
        "a cheap dispatch waited {cheap_elapsed:?} behind parked heavy work -- the reserved slot was not honored"
    );
    println!("[selfcheck-pool-fairness] cheap dispatch acquired the reserved slot in {cheap_waited_ms}ms while {heavy_slots_expected} heavy dispatches stayed parked");
    drop(cheap_guard);

    for _ in 0..heavy_slots_expected {
        let _ = release_tx.send(());
    }
    for t in heavy_threads {
        let _ = t.join();
    }
    let _ = extra_heavy.join();
    println!("[selfcheck-pool-fairness] witnessed live against the real SharedPluginPool: PASS");
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

    daemon::run_gm_dispatch_to_file(&root, &handle, "verbX", "taskY", "{}", &out_dir, 0);

    let entry_remains = daemon::in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_some();
    let out_written = out_dir.join("verbX-taskY.json").exists();
    println!("[selfcheck-inflight] entry_remains={entry_remains} out_written={out_written}");
    assert!(!entry_remains, "a completed dispatch must clear its own in-flight entry so the handoff/idle gates stop counting it");
    assert!(out_written, "the out file must still be written even when the dispatch itself errors (no registered plugin)");

    let _ = fs::remove_dir_all(&root);
    println!("[selfcheck-inflight] witnessed live against the real daemon dispatch path: PASS");
    Ok(())
}

fn selfcheck_spool_claim() -> anyhow::Result<()> {
    use std::fs;
    use std::time::{Duration, SystemTime};

    let in_dir = std::env::temp_dir()
        .join(format!("agentplug-selfcheck-claim-{}-{}", std::process::id(), agentplug_host::now_ms()))
        .join("codesearch");
    fs::create_dir_all(&in_dir)?;
    let txt = in_dir.join("s-1.txt");

    fs::write(&txt, b"")?;
    let empty_claimed = daemon::claim_spool_request_in_place(&txt).is_some();

    fs::write(&txt, br#"{"session_id":"s","query":"x"}"#)?;
    let fresh_claimed = daemon::claim_spool_request_in_place(&txt).is_some();

    let settled = SystemTime::now() - Duration::from_millis(2_000);
    fs::File::options().write(true).open(&txt)?.set_modified(settled)?;
    let settled_claimed = daemon::claim_spool_request_in_place(&txt).is_some();

    println!("[selfcheck-spool-claim] empty_claimed={empty_claimed} fresh_claimed={fresh_claimed} settled_claimed={settled_claimed}");
    assert!(!empty_claimed, "an empty in-file is the first half of a shell redirect, never a request: claiming it dispatches a torn body");
    assert!(!fresh_claimed, "an in-file written this instant may still be growing: claiming it dispatches a torn body");
    assert!(settled_claimed, "a non-empty in-file that has sat unmodified past the settle interval MUST still be claimable, or no dispatch ever runs");

    let _ = fs::remove_dir_all(in_dir.parent().unwrap_or(&in_dir));
    println!("[selfcheck-spool-claim] witnessed live against the real claim predicate: PASS");
    Ok(())
}

fn write_standalone_status(status_path: &std::path::Path) {
    use std::fs;
    let mut payload = match fs::read_to_string(status_path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
        Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => serde_json::json!({}),
    };
    payload["pid"] = serde_json::json!(std::process::id());
    payload["ts"] = serde_json::json!(agentplug_host::now_ms());
    payload["runtime"] = serde_json::json!("agentplug-runner-standalone");
    payload["daemon"] = serde_json::json!(false);
    payload["shared_process"] = serde_json::json!(false);
    let _ = fs::write(status_path, payload.to_string());
}

fn clear_standalone_status(status_path: &std::path::Path) {
    use std::fs;
    let Some(serde_json::Value::Object(mut map)) = fs::read_to_string(status_path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return;
    };
    map.remove("runtime");
    map.remove("daemon");
    map.remove("shared_process");
    map.insert("ts".to_string(), serde_json::json!(agentplug_host::now_ms()));
    let _ = fs::write(status_path, serde_json::Value::Object(map).to_string());
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
        if daemon::shared_daemon_is_serving() {
            clear_standalone_status(&status_path);
            eprintln!(
                "[agentplug] shared daemon is serving again -- standalone watcher for {} exiting between dispatches, leaving every unclaimed request in the spool for it",
                spool_dir.display()
            );
            return Ok(());
        }

        write_standalone_status(&status_path);

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
                    let Some(claim_path) = daemon::claim_spool_request_in_place(&path) else { continue };
                    let Ok(body) = fs::read_to_string(&claim_path) else {
                        let _ = fs::remove_file(&claim_path);
                        continue;
                    };
                    if body.trim().is_empty() {
                        let _ = fs::rename(&claim_path, &path);
                        continue;
                    }
                    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let result = project
                        .dispatch("gm", &verb, &body)
                        .unwrap_or_else(|e| serde_json::json!({"ok": false, "verb": verb, "error": e.to_string()}).to_string());
                    daemon::write_spool_out(&out_dir, &format!("{verb}-{stem}.json"), &result);
                    let _ = fs::remove_file(&claim_path);
                    work_done = true;
                }
            }
        }
        if !work_done {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}
