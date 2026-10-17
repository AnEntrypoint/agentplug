mod build_info;
mod daemon;
mod download;
mod dream_cycle;
mod logic;

use std::path::PathBuf;

use agentplug_host::{build_engine, ProjectPlugins};
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

fn release_bootstrap_status() -> serde_json::Value {
    serde_json::json!({
        "runner_version": env!("CARGO_PKG_VERSION"),
        "local_source_promotion_supported": false,
        "reason": "A checked-out source change is not an install artifact and cannot be promoted by the updater.",
        "supported_route": [
            "Publish the source through the normal repository release workflow.",
            "Let CI build and publish the next release artifact.",
            "Let the installed runner stage and verify that strictly newer artifact."
        ],
        "not_performed": [
            "Reading, exporting, or copying GitHub credentials.",
            "Replacing a runner binary or plugin from a local checkout."
        ]
    })
}

fn main() -> anyhow::Result<()> {
    agentplug_host::ensure_hidden_console();
    agentplug_host::spawn_housekeeping_timer();
    suppress_crash_dialogs();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        eprintln!(
            "[agentplug daemon] PANIC pid={} at {loc}: {info}",
            std::process::id()
        );
        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");

    let outcome = match cmd {
        "lua-call" => {
            let dir = args.get(2).cloned().unwrap_or_default();
            let function = args.get(3).cloned().unwrap_or_default();
            let input = args.get(4).cloned().unwrap_or_else(|| "{}".to_string());
            let result = (|| -> anyhow::Result<serde_json::Value> {
                let host = logic::LogicHost::load(std::path::Path::new(&dir))?;
                host.call(&function, &serde_json::from_str(&input)?)
            })();
            match result {
                Ok(value) => {
                    println!("{value}");
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("lua-call: {err:#}");
                    std::process::exit(1);
                }
            }
        }
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
            let cwd = agentplug_host::project_root(&cwd);
            let spool_dir = cwd.join(".gm").join("exec-spool");
            std::fs::create_dir_all(&spool_dir)?;

            if !daemon::claim_spool_launcher_slot(&spool_dir) {
                eprintln!(
                    "[agentplug] another spool launcher for {} is already live -- exiting",
                    cwd.display()
                );
                return Ok(());
            }
            let launcher_deadline = daemon::arm_spool_launcher_deadline();
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

            if daemon::ensure_daemon_running()? {
                eprintln!(
                    "[agentplug] registered {} with the shared system-wide daemon (converged after retry) -- no dedicated per-project process spawned",
                    cwd.display()
                );
                return Ok(());
            }

            let wasm = download::ensure_plugin_installed("gm", None)?;
            let content_hash = download::sha256_hex(&std::fs::read(&wasm)?);
            let engine = build_engine()?;
            let module =
                agentplug_host::load_module_file_backed(&engine, &wasm, "gm", &content_hash)?;
            let mut project = ProjectPlugins::new(cwd.clone());
            project.load_plugin(&engine, "gm", &module, &content_hash)?;
            launcher_deadline.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[agentplug] shared daemon did not publish a fresh heartbeat after two start attempts; serving {} with its single-process watcher until the shared daemon recovers",
                cwd.display()
            );
            run_spool_watcher_single_process(&mut project, &spool_dir)
        }
        "daemon-guard" => daemon::run_daemon_guard(),
        "daemon" => daemon::run_daemon(),
        "sweep-spool" => {
            let root = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("cwd unavailable"));
            daemon::sweep_orphaned_claims(&root);
            daemon::sweep_unconsumable_spool_files(&root);
            let reaped = daemon::reap_spool_out_files(&root, true);
            println!(
                "swept orphaned claims and unconsumable spool files under {} (reaped {reaped} answered out-file(s))",
                root.display()
            );
            Ok(())
        }
        "takeover" => {
            let version = args.get(2).cloned().unwrap_or_default();
            if version.is_empty() {
                eprintln!("usage: agentplug-runner takeover <version> [--force]");
                std::process::exit(1);
            }
            let force = args.iter().skip(3).any(|arg| arg == "--force");
            daemon::run_takeover(&version, force)
        }
        "dispatch" => {
            let plugin = args.get(2).cloned().unwrap_or_else(|| "gm".to_string());
            let verb = args.get(3).cloned().unwrap_or_default();
            let body = args.get(4).cloned().unwrap_or_else(|| "{}".to_string());
            let cwd = agentplug_host::project_root(&std::env::current_dir()?);

            match daemon::try_dispatch_via_daemon(&cwd, &plugin, &verb, &body) {
                daemon::DaemonDispatchOutcome::Answered(out) => {
                    println!("{out}");
                    return Ok(());
                }
                daemon::DaemonDispatchOutcome::ClaimedUnanswered(report) => {
                    println!("{report}");
                    std::process::exit(3);
                }
                daemon::DaemonDispatchOutcome::NeverClaimedRunLocally => {}
            }
            let wasm = download::ensure_plugin_installed(&plugin, None)?;
            let content_hash = download::sha256_hex(&std::fs::read(&wasm)?);
            let engine = build_engine()?;
            let module =
                agentplug_host::load_module_file_backed(&engine, &wasm, &plugin, &content_hash)?;
            let mut project = ProjectPlugins::new(cwd);
            project.load_plugin(&engine, &plugin, &module, &content_hash)?;
            let out = project.dispatch(&plugin, &verb, &body)?;
            let out = daemon::patch_update_available_from_escalation(&plugin, &verb, out);
            println!("{out}");
            Ok(())
        }
        "update-runner" => {
            match download::stage_runner_self_update()? {
                Some((staged, version)) => {
                    println!("staged verified runner {version} at {}", staged.display())
                }
                None => println!("no runner update to stage"),
            }
            Ok(())
        }
        "release-bootstrap-status" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&release_bootstrap_status())?
            );
            Ok(())
        }
        "--build-info" | "build-info" => {
            println!("{}", serde_json::to_string_pretty(&build_info::document())?);
            Ok(())
        }
        "pin-local-build" => {
            let sha = download::pin_local_build()?;
            println!("pinned the installed runner as a locally built binary (sha256 {sha}) -- the auto-updater will not overwrite it");
            Ok(())
        }
        "unpin-local-build" => {
            download::unpin_local_build()?;
            println!(
                "cleared the local-build pin -- the auto-updater may replace this runner again"
            );
            Ok(())
        }
        "--version" | "version" => {
            println!("agentplug-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "selfcheck-registry" => selfcheck_registry(),
        "selfcheck-inflight" => selfcheck_inflight_cleanup(),
        "selfcheck-idle-evict" => selfcheck_idle_evict(),
        "selfcheck-pool-fairness" => selfcheck_pool_fairness(),
        "selfcheck-git-admission" => selfcheck_git_admission(),
        "selfcheck-admission-hold-reclaim" => selfcheck_admission_hold_reclaim(),
        "selfcheck-lane-release" => selfcheck_lane_release(),
        "selfcheck-lane-fifo" => selfcheck_lane_fifo(),
        "selfcheck-lane-budget" => selfcheck_lane_budget(),
        "selfcheck-lane-hold-release" => selfcheck_lane_hold_release(),
        "selfcheck-lane-network-yield" => selfcheck_lane_network_yield(),
        "selfcheck-spool-claim" => selfcheck_spool_claim(),
        "selfcheck-browser-lease" => selfcheck_browser_lease(),
        "selfcheck-update-probes" => selfcheck_update_probes(),
        "component-dispatch" => component_dispatch(),
        "component-version" => component_version(),
        other => {
            eprintln!(
                "agentplug-runner: unknown command '{other}'. Usage: agentplug-runner <plugin <name> [version]|spool|daemon|takeover <version>|dispatch [plugin] <verb> [body]|sweep-spool [root]|update-runner|release-bootstrap-status|build-info|pin-local-build|unpin-local-build|selfcheck-registry|selfcheck-inflight|selfcheck-idle-evict|version>"
            );
            std::process::exit(1);
        }
    };
    agentplug_host::shutdown_shared_browsers();
    outcome
}

fn component_dispatch() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, _, wasm, verb, body, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: agentplug-runner component-dispatch <wasm> <verb> <body> [cwd]");
    };
    let cwd = match rest.first() {
        Some(cwd) => std::path::PathBuf::from(cwd),
        None => std::env::current_dir()?,
    };
    let engine = agentplug_host::build_engine()?;
    let bytes = std::fs::read(wasm)?;
    let reply = agentplug_host::call_component_dispatch(&engine, &bytes, &cwd, "plugkit", verb, body)?;
    println!("{reply}");
    Ok(())
}

fn component_version() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, _, wasm] = args.as_slice() else {
        anyhow::bail!("usage: agentplug-runner component-version <wasm>");
    };
    let engine = agentplug_host::build_engine()?;
    let bytes = std::fs::read(wasm)?;
    println!("{}", agentplug_host::call_component_version(&engine, &bytes, &std::env::current_dir()?)?);
    Ok(())
}

const SELFCHECK_SUCCESS_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "plugkit_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "plugkit_free") (param i32 i32))
  (func (export "plugin_call") (param i32 i32 i32 i32) (result i64) (i64.const 8589936640))
  (data (i32.const 2048) "ok")
)"#;

fn selfcheck_registry() -> anyhow::Result<()> {
    use agentplug_host::{
        note_shared_plugin_bytes_current, request_shared_store_swap,
        shared_plugin_slot_content_hashes, shared_plugin_swap_pending_hashes,
    };

    let engine = build_engine()?;
    let module = Module::new(&engine, SELFCHECK_SUCCESS_WAT)?;
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-registry-{}",
        std::process::id()
    ));
    let mut project = ProjectPlugins::new(root.clone());
    project.load_plugin(&engine, "gm", &module, "hash-a")?;
    let out = project.dispatch("gm", "probe", "{}")?;
    assert_eq!(
        out, "ok",
        "fresh slot must serve a real dispatch through the compiled module"
    );
    println!("[selfcheck-registry] fresh gm slot dispatched and returned {out:?}");

    let gm_slot_count = shared_plugin_slot_content_hashes("gm").len();
    let (evicted_now, deferred) = request_shared_store_swap("gm", "hash-a");
    println!("[selfcheck-registry] swap request against {gm_slot_count} idle slot(s): evicted_now={evicted_now} deferred={deferred}");
    assert_eq!(
        (evicted_now, deferred),
        (gm_slot_count, 0),
        "every idle slot holding the old hash must be evicted immediately, nothing deferred"
    );
    assert!(
        shared_plugin_slot_content_hashes("gm")
            .iter()
            .all(|h| h.is_none()),
        "evicted slot must show no content hash"
    );

    project.load_plugin(&engine, "gm", &module, "hash-b")?;
    let reinstantiated_reply = project.dispatch("gm", "probe", "{}")?;
    assert_eq!(
        reinstantiated_reply, "ok",
        "reinstantiated slot on the new hash must still serve real dispatches"
    );
    println!("[selfcheck-registry] slot reinstantiated on hash-b and served a second real dispatch: {reinstantiated_reply:?}");

    note_shared_plugin_bytes_current("gm", "hash-b");
    assert!(
        shared_plugin_swap_pending_hashes("gm").is_empty(),
        "marking hash-b current must leave no pending swap hashes"
    );
    println!(
        "[selfcheck-registry] all invariants witnessed live through real wasmtime dispatch: PASS"
    );
    Ok(())
}

fn selfcheck_pool_fairness() -> anyhow::Result<()> {
    use agentplug_host::{
        cost_class_for_dispatch, cost_class_for_verb, DispatchCostClass, SharedPluginPool,
    };
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    assert_eq!(
        cost_class_for_verb("code_index"),
        DispatchCostClass::Heavy,
        "code_index must classify as heavy"
    );
    assert_eq!(
        cost_class_for_verb("codesearch"),
        DispatchCostClass::Cheap,
        "the verb-only compatibility classifier must stay cheap"
    );
    assert_eq!(
        cost_class_for_dispatch("codesearch", r#"{"mode":"literal"}"#),
        DispatchCostClass::Cheap,
        "literal codesearch must preserve a slot for responsive tool work"
    );
    assert_eq!(
        cost_class_for_dispatch("codesearch", r#"{"mode":"regex"}"#),
        DispatchCostClass::Cheap,
        "regex codesearch must preserve a slot for responsive tool work"
    );
    assert_eq!(
        cost_class_for_dispatch("codesearch", r#"{"query":"semantic search"}"#),
        DispatchCostClass::Heavy,
        "default codesearch can rebuild and embed, so it must not consume the responsive slot"
    );
    assert_eq!(
        cost_class_for_dispatch(
            "codesearch",
            r#"{"glob":"AGENTS.md","query":"creationSnapPending"}"#
        ),
        DispatchCostClass::Cheap,
        "a codesearch scoped to one named file reads that file, so it must not queue on the store lane"
    );
    assert_eq!(
        cost_class_for_dispatch(
            "codesearch",
            r#"{"path":"AGENTS.md","query":"creationSnapPending"}"#
        ),
        DispatchCostClass::Cheap,
        "a codesearch scoped by path to one named file must not queue on the store lane"
    );
    assert_eq!(
        cost_class_for_dispatch(
            "codesearch",
            r#"{"paths":["AGENTS.md","src/main.cpp"],"query":"creationSnapPending"}"#
        ),
        DispatchCostClass::Cheap,
        "a codesearch scoped to a named file list must not queue on the store lane"
    );
    assert_eq!(
        cost_class_for_dispatch(
            "codesearch",
            r#"{"glob":"**/*.md","query":"creationSnapPending"}"#
        ),
        DispatchCostClass::Heavy,
        "a wildcard scope still walks and embeds a subtree, so it keeps the store lane"
    );
    assert_eq!(
        cost_class_for_verb("instruction"),
        DispatchCostClass::Cheap,
        "instruction must classify as cheap"
    );
    println!("[selfcheck-pool-fairness] verb classification: code_index heavy, codesearch/instruction cheap");

    const POOL_SIZE: usize = 4;
    let pool = Arc::new(SharedPluginPool::new("gm", POOL_SIZE));
    let heavy_slots_expected = POOL_SIZE - 2;

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
            let (_guard, _waited) = pool.acquire_within_for_class(
                SharedPluginPool::ACQUIRE_TIMEOUT_MS,
                DispatchCostClass::Heavy,
            );
            let _ = parked_tx.send(n);
            let _ = release_rx.lock().unwrap().recv();
        }));
    }
    for _ in 0..heavy_slots_expected {
        let parked = parked_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| anyhow::anyhow!("a heavy dispatch never acquired its slot: {e}"))?;
        println!(
            "[selfcheck-pool-fairness] heavy dispatch {parked} parked holding a real slot guard"
        );
    }

    let extra_heavy_pool = pool.clone();
    let (extra_admitted_tx, extra_admitted_rx) = mpsc::channel::<()>();
    let extra_heavy = std::thread::spawn(move || {
        let _admission = SharedPluginPool::admit(&extra_heavy_pool, DispatchCostClass::Heavy);
        let _ = extra_admitted_tx.send(());
    });
    assert!(
        extra_admitted_rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "a {}th heavy dispatch must NOT be admitted into a {POOL_SIZE}-slot pool -- the cheap admission budget stays reserved for responsive verbs",
        heavy_slots_expected + 1
    );
    println!("[selfcheck-pool-fairness] a further heavy dispatch is held at admission, so it cannot take the reserved slot");

    let cheap_start = Instant::now();
    let cheap_admission = SharedPluginPool::admit_within(
        &pool,
        DispatchCostClass::Cheap,
        Duration::from_millis(500),
    )
    .map_err(|report| {
        anyhow::anyhow!(
            "a cheap dispatch was refused admission behind parked heavy work: kind={} waited_ms={} limit={} in_flight={}",
            report.kind,
            report.waited_ms,
            report.limit,
            report.in_flight
        )
    })?;
    let (cheap_guard, cheap_waited_ms) = pool.acquire_within_for_class(
        SharedPluginPool::ACQUIRE_TIMEOUT_MS,
        DispatchCostClass::Cheap,
    );
    let cheap_elapsed = cheap_start.elapsed();
    assert!(
        cheap_elapsed < Duration::from_millis(500),
        "a cheap dispatch waited {cheap_elapsed:?} behind parked heavy work -- the reserved slot was not honored"
    );
    println!("[selfcheck-pool-fairness] cheap dispatch admitted and acquired the reserved slot in {cheap_waited_ms}ms while {heavy_slots_expected} heavy dispatches stayed parked");
    drop(cheap_guard);
    drop(cheap_admission);

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

fn selfcheck_admission_hold_reclaim() -> anyhow::Result<()> {
    use agentplug_host::registry::admission_holds;
    use agentplug_host::{DispatchCostClass, SharedPluginPool};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    const NAME: &str = "selfcheck-admission-hold-reclaim";
    const POOL_SLOTS: usize = 4;
    std::env::set_var("AGENTPLUG_ADMISSION_HOLD_MAX_MS", "300");
    let pool = Arc::new(SharedPluginPool::new("gm", POOL_SLOTS));
    let general_limit = POOL_SLOTS - (POOL_SLOTS / 4).max(1);
    let mut parked = Vec::new();
    for _ in 0..general_limit {
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let held = pool.clone();
        let handle = std::thread::spawn(move || {
            let Ok(admission) =
                SharedPluginPool::admit_within(&held, DispatchCostClass::Cheap, Duration::from_secs(5))
            else {
                let _ = ready_tx.send(false);
                return;
            };
            let (slot, _) = held.acquire_within_for_class(
                SharedPluginPool::ACQUIRE_TIMEOUT_MS,
                DispatchCostClass::Cheap,
            );
            let _ = ready_tx.send(true);
            let _ = release_rx.recv();
            drop(slot);
            drop(admission);
        });
        parked.push((ready_rx, release_tx, handle));
    }
    let outcome = (|| -> Result<String, String> {
        let mut all_parked = true;
        for (ready_rx, _, _) in &parked {
            all_parked &= ready_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false);
        }
        if !all_parked {
            return Err(format!(
                "only part of the {general_limit} general cheap dispatches could be parked"
            ));
        }
        if SharedPluginPool::admit_within(&pool, DispatchCostClass::Cheap, Duration::from_millis(300))
            .is_ok()
        {
            return Err(format!(
                "the general band admitted a {}th cheap dispatch while every holder was still inside the hold bound",
                general_limit + 1
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
        let (_slot, _) = pool.acquire_within_for_class(
            SharedPluginPool::ACQUIRE_TIMEOUT_MS,
            DispatchCostClass::Cheap,
        );
        let started = Instant::now();
        let reclaimed = SharedPluginPool::admit_within(
            &pool,
            DispatchCostClass::Cheap,
            Duration::from_secs(2),
        )
        .map_err(|report| {
            format!(
                "a cheap dispatch still starved at admission after the hold bound passed: waited_ms={} limit={} in_flight={} holders={}",
                report.waited_ms, report.limit, report.in_flight, report.holders
            )
        })?;
        let reclaim_ms = started.elapsed().as_millis();
        if reclaim_ms >= 1_000 {
            return Err(format!(
                "the reclaimed admission took {reclaim_ms}ms -- it waited out its patience budget instead of taking the over-age counter back"
            ));
        }
        let git = SharedPluginPool::admit_within_lane(
            &pool,
            DispatchCostClass::Cheap,
            Some("git"),
            Duration::from_millis(500),
        )
        .map_err(|report| {
            format!(
                "a git-lane dispatch was refused admission at the production pool size: waited_ms={} limit={} in_flight={} holders={}",
                report.waited_ms, report.limit, report.in_flight, report.holders
            )
        })?;
        let kinds = admission_holds()
            .iter()
            .map(|hold| hold.kind.to_string())
            .collect::<Vec<_>>();
        if !kinds.iter().any(|kind| kind == "general") {
            return Err(format!(
                "admission_holds() listed no general-band hold ({}) -- the stall would still be a phantom",
                kinds.join(", ")
            ));
        }
        drop(git);
        drop(reclaimed);
        drop(_slot);
        Ok(format!(
            "at the production pool size ({POOL_SLOTS} slots, general band {general_limit}) a cheap dispatch was admitted in {reclaim_ms}ms by taking back an over-age counter from {general_limit} parked holders; the git band admitted a git-lane dispatch; holds are listed as {}",
            kinds.join(", ")
        ))
    })();
    for (_, release_tx, handle) in parked {
        let _ = release_tx.send(());
        let _ = handle.join();
    }
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real SharedPluginPool: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_fail(name: &str, reason: &str) -> anyhow::Result<()> {
    println!("RESULT: FAIL {reason}");
    anyhow::bail!("{name}: {reason}")
}

fn selfcheck_git_admission() -> anyhow::Result<()> {
    use agentplug_host::{AdmissionBand, DispatchCostClass, SharedPluginPool};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    const NAME: &str = "selfcheck-git-admission";
    const POOL_SLOTS: usize = 8;
    const GIT_BOUND: Duration = Duration::from_secs(10);
    let general_limit = POOL_SLOTS - (POOL_SLOTS / 4).max(1);
    let pool = Arc::new(SharedPluginPool::new("gm", POOL_SLOTS));
    let mut parked = Vec::new();
    for _ in 0..general_limit {
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let held = pool.clone();
        let handle = std::thread::spawn(move || {
            let Ok(admission) = SharedPluginPool::admit_within(
                &held,
                DispatchCostClass::Cheap,
                Duration::from_secs(5),
            ) else {
                let _ = ready_tx.send(false);
                return;
            };
            let (slot, _) = held.acquire_within_for_class(
                SharedPluginPool::ACQUIRE_TIMEOUT_MS,
                DispatchCostClass::Cheap,
            );
            let _ = ready_tx.send(true);
            let _ = release_rx.recv();
            drop(slot);
            drop(admission);
        });
        parked.push((ready_rx, release_tx, handle));
    }
    let outcome = (|| -> Result<String, String> {
        let mut all_parked = true;
        for (ready_rx, _, _) in &parked {
            all_parked &= ready_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false);
        }
        if !all_parked {
            return Err(format!(
                "only part of the {general_limit} general cheap dispatches could be parked"
            ));
        }
        if SharedPluginPool::admit_within(&pool, DispatchCostClass::Cheap, Duration::from_millis(300))
            .is_ok()
        {
            return Err(format!(
                "the general band admitted more than {general_limit} cheap dispatches"
            ));
        }
        let started = Instant::now();
        let git = SharedPluginPool::admit_within_lane(
            &pool,
            DispatchCostClass::Cheap,
            Some("git"),
            GIT_BOUND,
        );
        let git_ms = started.elapsed().as_millis();
        let git = match git {
            Ok(admission) => admission,
            Err(report) => {
                return Err(format!(
                    "git-lane cheap dispatch refused admission after {}ms behind {} of {} general admissions",
                    report.waited_ms, report.in_flight, report.limit
                ))
            }
        };
        let record = agentplug_host::take_last_admission();
        let band = record.map(|record| record.band);
        let band_label = band.map_or("none", AdmissionBand::as_str);
        if band != Some(AdmissionBand::GitBand) {
            return Err(format!(
                "git-lane cheap dispatch was admitted through the {band_label} band, not the git band"
            ));
        }
        let annotated = daemon::annotate_admission_on_reply(
            r#"{"ok":true,"verb":"git_branch"}"#.to_string(),
            record,
        );
        let reply: serde_json::Value =
            serde_json::from_str(&annotated).unwrap_or(serde_json::Value::Null);
        if reply["admission_decision"] != "admitted"
            || reply["admission_band"] != "git-band"
            || !reply["admission_waited_ms"].is_u64()
        {
            return Err(format!(
                "the git reply did not name its admission decision, band and wait: {annotated}"
            ));
        }
        let (git_slot, _) = pool.acquire_within_for_class(
            SharedPluginPool::ACQUIRE_TIMEOUT_MS,
            DispatchCostClass::Cheap,
        );
        let short_started = Instant::now();
        let (short_slot, _) = pool.acquire_within_for_class(
            SharedPluginPool::ACQUIRE_TIMEOUT_MS,
            DispatchCostClass::Short,
        );
        let short_ms = short_started.elapsed().as_millis();
        if short_ms >= 2_000 {
            return Err(format!(
                "a short verb waited {short_ms}ms for a physical slot behind the git dispatch"
            ));
        }
        drop(short_slot);
        if SharedPluginPool::admit_within_lane(
            &pool,
            DispatchCostClass::Cheap,
            Some("git"),
            Duration::from_millis(300),
        )
        .is_ok()
        {
            return Err("the git band admitted a second git-lane dispatch".into());
        }
        drop(git_slot);
        drop(git);
        if SharedPluginPool::admit_within_lane(&pool, DispatchCostClass::Cheap, Some("git"), GIT_BOUND)
            .is_err()
        {
            return Err("the git band did not free its slot after the first git dispatch".into());
        }
        Ok(format!(
            "git-lane cheap dispatch admitted via the {band_label} admission in {git_ms}ms while {general_limit} general cheap dispatches hold the band; a short verb got a slot in {short_ms}ms; a second git dispatch was refused within its bound"
        ))
    })();
    for (_, release_tx, handle) in parked {
        let _ = release_tx.send(());
        let _ = handle.join();
    }
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real SharedPluginPool: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_lane_release() -> anyhow::Result<()> {
    use agentplug_host::GmFairnessGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    const NAME: &str = "selfcheck-lane-release";
    std::env::set_var("AGENTPLUG_LANE_RELEASE_MIN_AGE_MS", "200");
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lane-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    std::fs::create_dir_all(&root)?;
    let (held_tx, held_rx) = mpsc::channel::<bool>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder_root = root.clone();
    let holder = std::thread::spawn(move || {
        let guard = GmFairnessGuard::acquire_within(
            &holder_root,
            "prd-add",
            "{}",
            "task-dead",
            Duration::from_secs(5),
        );
        let _ = held_tx.send(guard.is_ok());
        let _ = release_rx.recv();
        drop(guard);
    });
    let outcome = (|| -> Result<String, String> {
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the abandoned holder could not take the project lane".into());
        }
        match GmFairnessGuard::acquire_within(&root, "prd-add", "{}", "task-2", Duration::from_millis(500)) {
            Ok(_) => return Err("the lane was free while task-dead still held it".into()),
            Err(report) if report.lane_holder_task.as_deref() != Some("task-dead") => {
                return Err(format!(
                    "the waiter was refused without naming the holder (holder {:?})",
                    report.lane_holder_task
                ))
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(250));
        daemon::release_abandoned_project_lanes();
        match GmFairnessGuard::acquire_within(&root, "prd-add", "{}", "task-3", Duration::from_secs(2)) {
            Ok(guard) => {
                drop(guard);
                Ok("orphan sweep released the lane held by task-dead".into())
            }
            Err(report) => Err(format!(
                "the orphan sweep left the lane held by {:?} although its requester has no live lease",
                report.lane_holder_task
            )),
        }
    })();
    let _ = release_tx.send(());
    let _ = holder.join();

    let live_root = root.join("live-project");
    std::fs::create_dir_all(&live_root)?;
    let (held2_tx, held2_rx) = mpsc::channel::<bool>();
    let (release2_tx, release2_rx) = mpsc::channel::<()>();
    let holder2_root = live_root.clone();
    let holder2 = std::thread::spawn(move || {
        let guard = GmFairnessGuard::acquire_within(
            &holder2_root,
            "git_commit",
            "{}",
            "git-task-dead",
            Duration::from_secs(5),
        );
        let _ = held2_tx.send(guard.is_ok());
        let _ = release2_rx.recv();
        drop(guard);
    });
    let outcome2 = (|| -> Result<String, String> {
        if !held2_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the dead git holder could not take the project lane".into());
        }
        match GmFairnessGuard::acquire_within(
            &live_root,
            "git_commit",
            "{}",
            "git-task-2",
            Duration::from_millis(500),
        ) {
            Ok(_) => return Err("the git lane was free while git-task-dead still held it".into()),
            Err(report) if report.lane_holder_task.as_deref() != Some("git-task-dead") => {
                return Err(format!(
                    "the waiter was refused without naming the holder (holder {:?})",
                    report.lane_holder_task
                ))
            }
            Err(report) if report.lane_holder_verb.as_deref() != Some("git_commit") => {
                return Err(format!(
                    "the waiter was refused without naming the holder verb (verb {:?})",
                    report.lane_holder_verb
                ))
            }
            Err(report) if report.lane_holder_held_ms == 0 => {
                return Err(
                    "the waiter was refused without reporting how long the lane was held".into(),
                )
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(250));
        let live_reaped = agentplug_host::release_dead_lane_owners(
            200,
            &|_r: &std::path::Path, _l: &'static str, _v: &str, _t: &str| true,
        );
        if live_reaped.iter().any(|o| o.task == "git-task-dead") {
            return Err("the reap freed a lane whose holder dispatch is still live".into());
        }
        let reaped = agentplug_host::release_dead_lane_owners(
            200,
            &|_r: &std::path::Path, _l: &'static str, _v: &str, _t: &str| false,
        );
        if !reaped
            .iter()
            .any(|o| o.task == "git-task-dead" && o.verb == "git_commit" && o.lane == "git")
        {
            return Err(format!(
                "the per-holder reap did not free a git lane inside a live project (reaped {:?})",
                reaped.iter().map(|o| o.task.clone()).collect::<Vec<_>>()
            ));
        }
        let successor = match GmFairnessGuard::acquire_within(
            &live_root,
            "git_commit",
            "{}",
            "git-task-3",
            Duration::from_secs(2),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "the lane stayed held by {:?} after the per-holder reap",
                    report.lane_holder_task
                ))
            }
        };
        let _ = release2_tx.send(());
        let _ = holder2.join();
        match GmFairnessGuard::acquire_within(
            &live_root,
            "git_commit",
            "{}",
            "git-task-4",
            Duration::from_millis(500),
        ) {
            Ok(_) => {
                return Err("the old holder's late drop released the successor's lane".into())
            }
            Err(report) if report.lane_holder_task.as_deref() != Some("git-task-3") => {
                return Err(format!(
                    "the old holder's late drop left the lane owned by {:?}",
                    report.lane_holder_task
                ))
            }
            Err(_) => {}
        }
        drop(successor);
        Ok("per-holder reap released a git lane inside a live project, and the dead holder's late drop left its successor alone".into())
    })();
    let _ = release2_tx.send(());

    let panic_root = root.join("panicking-project");
    std::fs::create_dir_all(&panic_root)?;
    let panic_root2 = panic_root.clone();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = GmFairnessGuard::acquire_within(
            &panic_root2,
            "git_commit",
            "{}",
            "git-task-panic",
            Duration::from_secs(2),
        )
        .map_err(|_| "the panicking holder could not take the git lane")?;
        panic!("the dispatch died mid-git");
        #[allow(unreachable_code)]
        Ok::<(), &str>(())
    }));
    let outcome3 = (|| -> Result<String, String> {
        if !panicked.is_err() {
            return Err("the holder meant to panic did not panic".into());
        }
        match GmFairnessGuard::acquire_within(
            &panic_root,
            "git_commit",
            "{}",
            "git-task-after-panic",
            Duration::from_secs(2),
        ) {
            Ok(_) => Ok("a panicking dispatch released its lane through Drop".into()),
            Err(report) => Err(format!(
                "a panicking dispatch left the lane held by {:?}",
                report.lane_holder_task
            )),
        }
    })();

    let outcome = match (outcome, outcome2, outcome3) {
        (Ok(a), Ok(b), Ok(c)) => Ok(format!("{a}; {b}; {c}")),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
    };

    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real GmFairnessGuard: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_lane_network_yield() -> anyhow::Result<()> {
    use agentplug_host::GmFairnessGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    const NAME: &str = "selfcheck-lane-network-yield";
    std::env::set_var("AGENTPLUG_LANE_NETWORK_YIELD_MS", "1000");
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lane-yield-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    std::fs::create_dir_all(&root)?;
    let root_a = root.join("a");
    let root_b = root.join("b");
    let root_c = root.join("c");
    std::fs::create_dir_all(&root_a)?;
    std::fs::create_dir_all(&root_b)?;
    std::fs::create_dir_all(&root_c)?;

    const PUSH_BODY: &str = "{\"SESSION_ID\":\"yield-push\",\"rev\":\"HEAD\"}";
    const COMMIT_BODY: &str = "{\"SESSION_ID\":\"yield-commit\",\"paths\":[\"a\"]}";

    fn hold(root: &std::path::Path, task: &'static str) -> (mpsc::Receiver<bool>, mpsc::Sender<()>) {
        let (held_tx, held_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder_root = root.to_path_buf();
        std::thread::spawn(move || {
            let guard = GmFairnessGuard::acquire_within(
                &holder_root,
                "git_commit",
                "{}",
                task,
                Duration::from_secs(10),
            );
            let _ = held_tx.send(guard.is_ok());
            let _ = release_rx.recv();
            drop(guard);
        });
        (held_rx, release_tx)
    }

    let outcome = (|| -> Result<String, String> {
        let (held_rx, release_tx) = hold(&root_a, "holder-yield");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the holder could not take the project git lane".into());
        }
        match GmFairnessGuard::acquire_within(
            &root_a,
            "git_push",
            PUSH_BODY,
            "push-1",
            Duration::from_millis(150),
        ) {
            Ok(_) => return Err("the push took the lane while the holder still held it".into()),
            Err(report) if report.lane_position != Some(1) => {
                return Err(format!(
                    "the push starved at position {:?}, expected 1",
                    report.lane_position
                ))
            }
            Err(_) => {}
        }
        match GmFairnessGuard::acquire_within(
            &root_a,
            "git_commit",
            COMMIT_BODY,
            "commit-1",
            Duration::from_millis(150),
        ) {
            Ok(_) => return Err("the commit took the lane while the holder still held it".into()),
            Err(report) if report.lane_position != Some(2) => {
                return Err(format!(
                    "the commit starved at position {:?}, expected 2 behind the push",
                    report.lane_position
                ))
            }
            Err(_) => {}
        }
        let _ = release_tx.send(());
        let commit = match GmFairnessGuard::acquire_within(
            &root_a,
            "git_commit",
            COMMIT_BODY,
            "commit-1-admitted",
            Duration::from_secs(3),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "the commit behind a fresh push was not admitted ahead of it (lane still held by {:?})",
                    report.lane_holder_task
                ))
            }
        };
        drop(commit);

        let (held_rx, release_tx) = hold(&root_b, "holder-fifo");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the second holder could not take the project git lane".into());
        }
        let _ = GmFairnessGuard::acquire_within(
            &root_b,
            "git_commit",
            "{\"SESSION_ID\":\"yield-commit-first\"}",
            "commit-first",
            Duration::from_millis(150),
        );
        let _ = GmFairnessGuard::acquire_within(
            &root_b,
            "git_push",
            "{\"SESSION_ID\":\"yield-push-behind\"}",
            "push-behind",
            Duration::from_millis(150),
        );
        let _ = release_tx.send(());
        let first = match GmFairnessGuard::acquire_within(
            &root_b,
            "git_commit",
            "{\"SESSION_ID\":\"yield-commit-first\"}",
            "commit-first-admitted",
            Duration::from_secs(3),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "a push overtook the commit queued ahead of it (lane held by {:?})",
                    report.lane_holder_task
                ))
            }
        };
        drop(first);

        let (held_rx, release_tx) = hold(&root_c, "holder-aged");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the third holder could not take the project git lane".into());
        }
        let _ = GmFairnessGuard::acquire_within(
            &root_c,
            "git_push",
            "{\"SESSION_ID\":\"yield-push-aged\"}",
            "push-aged",
            Duration::from_millis(150),
        );
        let _ = GmFairnessGuard::acquire_within(
            &root_c,
            "git_commit",
            "{\"SESSION_ID\":\"yield-commit-late\"}",
            "commit-late",
            Duration::from_millis(150),
        );
        std::thread::sleep(Duration::from_millis(1200));
        let _ = release_tx.send(());
        match GmFairnessGuard::acquire_within(
            &root_c,
            "git_commit",
            "{\"SESSION_ID\":\"yield-commit-late\"}",
            "commit-late-admitted",
            Duration::from_millis(600),
        ) {
            Ok(_) => {
                return Err(
                    "a push that had waited out its yield budget still yielded to the commit behind it"
                        .into(),
                )
            }
            Err(_) => {}
        }
        match GmFairnessGuard::acquire_within(
            &root_c,
            "git_push",
            "{\"SESSION_ID\":\"yield-push-aged\"}",
            "push-aged-admitted",
            Duration::from_secs(3),
        ) {
            Ok(guard) => drop(guard),
            Err(report) => {
                return Err(format!(
                    "the aged push was not admitted once it stopped yielding (lane held by {:?})",
                    report.lane_holder_task
                ))
            }
        }

        Ok("a commit was admitted ahead of a fresh push, a push never overtook a commit ahead of it, and a push that waited out its yield budget stopped yielding and took the lane".into())
    })();

    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real GmFairnessGuard: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_lane_budget() -> anyhow::Result<()> {
    use crate::daemon::{
        lane_wait_ceiling_ms, lane_wait_default_ms, lane_wait_max, split_caller_lane_wait_ms,
    };
    use agentplug_host::GmFairnessGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    const NAME: &str = "selfcheck-lane-budget";
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lane-budget-{}",
        std::process::id()
    ));
    let root_budget = root.join("budget");
    std::fs::create_dir_all(&root_budget)?;

    const BODY_WITH_BUDGET: &str =
        "{\"SESSION_ID\":\"budget\",\"paths\":[\"a\"],\"_agentplug_lane_wait_ms\":1200}";
    const BODY_BUDGET_FIRST: &str =
        "{\"_agentplug_lane_wait_ms\":1200,\"SESSION_ID\":\"budget\",\"paths\":[\"a\"]}";
    const BODY_STRIPPED: &str = "{\"SESSION_ID\":\"budget\",\"paths\":[\"a\"]}";

    fn hold(root: &std::path::Path, task: &'static str) -> (mpsc::Receiver<bool>, mpsc::Sender<()>) {
        let (held_tx, held_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder_root = root.to_path_buf();
        std::thread::spawn(move || {
            let guard = GmFairnessGuard::acquire_within(
                &holder_root,
                "git_commit",
                "{}",
                task,
                Duration::from_secs(30),
            );
            let _ = held_tx.send(guard.is_ok());
            let _ = release_rx.recv();
            drop(guard);
        });
        (held_rx, release_tx)
    }

    let outcome = (|| -> Result<String, String> {
        let (stripped, budget) = split_caller_lane_wait_ms(BODY_WITH_BUDGET);
        if stripped != BODY_STRIPPED {
            return Err(format!(
                "the reserved member was not removed byte-exactly: got {stripped:?}"
            ));
        }
        if budget != Some(1200) {
            return Err(format!("the caller's budget was not read: got {budget:?}"));
        }
        let (stripped_first, budget_first) = split_caller_lane_wait_ms(BODY_BUDGET_FIRST);
        if stripped_first != BODY_STRIPPED || budget_first != Some(1200) {
            return Err(format!(
                "a leading reserved member was not removed byte-exactly: got {stripped_first:?} / {budget_first:?}"
            ));
        }
        if strip_without_budget() {
            return Err("a body with no reserved member was altered".into());
        }
        let (stripped_public, budget_public) = split_caller_lane_wait_ms(
            "{\"SESSION_ID\":\"budget\",\"paths\":[\"a\"],\"lane_wait_ms\":9000}",
        );
        if stripped_public != BODY_STRIPPED || budget_public != Some(9_000) {
            return Err(format!(
                "a caller-written lane_wait_ms was not read and stripped: got {stripped_public:?} / {budget_public:?}"
            ));
        }
        if split_caller_lane_wait_ms(
            "{\"SESSION_ID\":\"budget\",\"_agentplug_lane_wait_ms\":1200,\"lane_wait_ms\":9000}",
        )
        .1
            != Some(1_200)
        {
            return Err("a caller-written lane_wait_ms overrode the reserved budget".into());
        }

        if lane_wait_max("git_commit", Some(0)) != Duration::from_millis(1_000) {
            return Err(format!(
                "a sub-second caller budget was not raised to the floor: got {:?}",
                lane_wait_max("git_commit", Some(0))
            ));
        }
        if lane_wait_max("git_commit", Some(1_200)) != Duration::from_millis(1_200) {
            return Err(format!(
                "the caller's 1200ms budget did not bound the wait: got {:?}",
                lane_wait_max("git_commit", Some(1_200))
            ));
        }
        if lane_wait_max("git_commit", Some(u64::MAX)) != lane_wait_max("git_commit", None) {
            return Err(
                "a caller budget larger than the lane ceiling was honoured past that ceiling".into(),
            );
        }

        let codesearch_default = lane_wait_default_ms("codesearch");
        let codesearch_ceiling = lane_wait_ceiling_ms("codesearch");
        if codesearch_default >= codesearch_ceiling {
            return Err(format!(
                "codesearch's default wait ({codesearch_default}ms) is not below its ceiling ({codesearch_ceiling}ms) -- a caller that states no budget would no longer be answered promptly"
            ));
        }
        if lane_wait_max("codesearch", None) != Duration::from_millis(codesearch_default) {
            return Err(format!(
                "a codesearch with no stated budget did not get the short default: got {:?}",
                lane_wait_max("codesearch", None)
            ));
        }
        if lane_wait_max("codesearch", Some(240_000))
            != Duration::from_millis(codesearch_ceiling.min(240_000))
        {
            return Err(format!(
                "a codesearch caller that asked for 240s did not buy the wait: got {:?} of a {codesearch_ceiling}ms ceiling",
                lane_wait_max("codesearch", Some(240_000))
            ));
        }

        let (held_rx, release_tx) = hold(&root_budget, "holder-budget");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the holder could not take the project git lane".into());
        }
        let waited_ms = match GmFairnessGuard::acquire_within(
            &root_budget,
            "git_commit",
            BODY_STRIPPED,
            "budget-waiter",
            lane_wait_max("git_commit", budget),
        ) {
            Ok(_) => {
                return Err("the waiter took the lane while the holder still held it".into())
            }
            Err(report) => report.waited_ms,
        };
        if waited_ms < 1_200 {
            return Err(format!(
                "the waiter was starved after {waited_ms}ms, before its 1200ms budget ran out"
            ));
        }
        if waited_ms > 5_000 {
            return Err(format!(
                "the waiter waited {waited_ms}ms on a 1200ms budget -- the caller's budget did not bound the wait"
            ));
        }
        let _ = release_tx.send(());
        Ok(format!(
            "the reserved member is stripped byte-exactly from either side of the body, a caller-written lane_wait_ms is read and stripped too, the caller's budget bounds the lane wait (starved at {waited_ms}ms on a 1200ms budget) between a 1000ms floor and the lane ceiling, and codesearch's short {codesearch_default}ms default no longer clips a caller that asked for 240s (ceiling {codesearch_ceiling}ms)"
        ))
    })();

    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real GmFairnessGuard: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn strip_without_budget() -> bool {
    use crate::daemon::split_caller_lane_wait_ms;
    let body = "{\"SESSION_ID\":\"plain\",\"paths\":[\"a\"]}";
    let (stripped, budget) = split_caller_lane_wait_ms(body);
    stripped != body || budget.is_some()
}

fn selfcheck_lane_hold_release() -> anyhow::Result<()> {
    use crate::daemon::{
        in_flight_map, lane_holder_stuck_reason, note_dispatch_stage, release_stuck_lane_holders,
        DispatchStage, InFlightHandle,
    };
    use agentplug_host::GmFairnessGuard;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    const NAME: &str = "selfcheck-lane-hold-release";
    std::env::set_var("AGENTPLUG_LANE_HOLD_STUCK_MIN_MS", "400");
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lane-hold-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    let root_stuck = root.join("stuck");
    let root_running = root.join("running");
    let root_running_past = root.join("running-past-max");
    let root_worker_gone = root.join("worker-gone");
    std::fs::create_dir_all(&root_stuck)?;
    std::fs::create_dir_all(&root_running)?;
    std::fs::create_dir_all(&root_running_past)?;
    std::fs::create_dir_all(&root_worker_gone)?;

    fn hold_reporting_ownership(
        root: &std::path::Path,
        task: &'static str,
    ) -> (mpsc::Receiver<bool>, mpsc::Sender<()>, mpsc::Receiver<bool>) {
        let (held_tx, held_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (owns_tx, owns_rx) = mpsc::channel::<bool>();
        let holder_root = root.to_path_buf();
        std::thread::spawn(move || {
            let guard = GmFairnessGuard::acquire_within(
                &holder_root,
                "git_commit",
                "{}",
                task,
                Duration::from_secs(30),
            );
            let _ = held_tx.send(guard.is_ok());
            let _ = release_rx.recv();
            let _ = owns_tx.send(guard.as_ref().map(|g| g.still_owns()).unwrap_or(false));
            drop(guard);
        });
        (held_rx, release_tx, owns_rx)
    }

    let register_worker = |root: &std::path::Path, task: &str| {
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (
                    root.to_path_buf(),
                    "git_commit".to_string(),
                    task.to_string(),
                ),
                InFlightHandle {
                    detach: Arc::new(AtomicBool::new(false)),
                },
            )
    };
    let unregister_worker = |root: &std::path::Path, task: &str| {
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(
                root.to_path_buf(),
                "git_commit".to_string(),
                task.to_string(),
            ))
    };

    let outcome = (|| -> Result<String, String> {
        let (held_rx, release_tx, owns_rx) = hold_reporting_ownership(&root_stuck, "holder-stuck");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the stuck holder could not take the project git lane".into());
        }
        note_dispatch_stage(
            &root_stuck,
            "git_commit",
            "holder-stuck",
            DispatchStage::WaitingForToolQueue,
            Some("git"),
        );
        std::thread::sleep(Duration::from_millis(700));
        let released = release_stuck_lane_holders(100);
        let reason = released
            .iter()
            .find(|released| released.task == "holder-stuck")
            .map(|released| released.reason);
        if reason != Some("holder_stuck_before_running") {
            return Err(format!(
                "a holder stuck before running was not released with a named reason: got {reason:?} over {released:?} released hold(s)"
            ));
        }
        let claimer = match GmFairnessGuard::acquire_within(
            &root_stuck,
            "git_commit",
            "{}",
            "claimer-after-release",
            Duration::from_secs(3),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "the lane was not free after the release (still held by {:?})",
                    report.lane_holder_task
                ))
            }
        };
        let _ = release_tx.send(());
        if owns_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(true) {
            return Err(
                "the displaced holder still believed it owned the lane -- it would have run on a lane it no longer holds"
                    .into(),
            );
        }
        drop(claimer);

        let (held_rx, release_tx, owns_rx) =
            hold_reporting_ownership(&root_running, "holder-running");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the running holder could not take the project git lane".into());
        }
        register_worker(&root_running, "holder-running");
        note_dispatch_stage(
            &root_running,
            "git_commit",
            "holder-running",
            DispatchStage::Running,
            Some("git"),
        );
        std::thread::sleep(Duration::from_millis(700));
        if lane_holder_stuck_reason(&root_running, "git", "git_commit", "holder-running").is_some() {
            return Err("a running holder with no waiters behind it was classified as stuck".into());
        }
        let released = release_stuck_lane_holders(100);
        if released.iter().any(|released| released.task == "holder-running") {
            return Err("a running holder was released from the lane".into());
        }
        let _ = release_tx.send(());
        if !owns_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the running holder lost ownership of its lane".into());
        }
        unregister_worker(&root_running, "holder-running");

        std::env::set_var("AGENTPLUG_LANE_HOLD_RUNNING_MAX_MS", "400");
        let (held_rx, release_tx, owns_rx) =
            hold_reporting_ownership(&root_running_past, "holder-running-past-max");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the long-running holder could not take the project git lane".into());
        }
        register_worker(&root_running_past, "holder-running-past-max");
        note_dispatch_stage(
            &root_running_past,
            "git_commit",
            "holder-running-past-max",
            DispatchStage::Running,
            Some("git"),
        );
        let waiter_root = root_running_past.clone();
        let (waiter_tx, waiter_rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let guard = GmFairnessGuard::acquire_within(
                &waiter_root,
                "git_commit",
                "{}",
                "waiter-behind-long-runner",
                Duration::from_secs(30),
            );
            let _ = waiter_tx.send(guard.is_ok());
        });
        std::thread::sleep(Duration::from_millis(700));
        let reason_past = lane_holder_stuck_reason(
            &root_running_past,
            "git",
            "git_commit",
            "holder-running-past-max",
        );
        if reason_past != Some("holder_running_past_max") {
            return Err(format!(
                "a holder running past the ceiling with a waiter behind it was not classified as stuck: got {reason_past:?}"
            ));
        }
        let released = release_stuck_lane_holders(100);
        if !released
            .iter()
            .any(|released| released.task == "holder-running-past-max")
        {
            return Err("a holder running past the ceiling with a waiter behind it was not released from the lane".into());
        }
        if !waiter_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err(
                "the lane did not drain: the waiter queued behind the long-running holder never got it"
                    .into(),
            );
        }
        let _ = release_tx.send(());
        if owns_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(true) {
            return Err(
                "the released long-running holder still believed it owned the lane -- the lane did not really drain"
                    .into(),
            );
        }
        unregister_worker(&root_running_past, "holder-running-past-max");

        let (held_rx, release_tx, owns_rx) =
            hold_reporting_ownership(&root_worker_gone, "holder-worker-gone");
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the holder whose worker is gone could not take the project git lane".into());
        }
        note_dispatch_stage(
            &root_worker_gone,
            "git_commit",
            "holder-worker-gone",
            DispatchStage::Running,
            Some("git"),
        );
        std::thread::sleep(Duration::from_millis(700));
        let reason_gone =
            lane_holder_stuck_reason(&root_worker_gone, "git", "git_commit", "holder-worker-gone");
        if reason_gone != Some("holder_worker_not_alive") {
            return Err(format!(
                "a hold whose worker is gone was not classified as stuck: got {reason_gone:?}"
            ));
        }
        let released = release_stuck_lane_holders(100);
        if !released
            .iter()
            .any(|released| released.task == "holder-worker-gone")
        {
            return Err(
                "a hold whose worker is gone was not released from the lane".into(),
            );
        }
        if lane_holder_stuck_reason(&root_worker_gone, "git", "git_commit", "holder-worker-gone")
            != Some("holder_dispatch_gone")
        {
            return Err(
                "the dead worker's dispatch is still named as running after the release -- the orphan sweep cannot reap a claim the ledger calls running"
                    .into(),
            );
        }
        let claimer = match GmFairnessGuard::acquire_within(
            &root_worker_gone,
            "git_commit",
            "{}",
            "claimer-after-worker-gone",
            Duration::from_secs(3),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "the lane was not free after the dead worker's hold was released (still held by {:?})",
                    report.lane_holder_task
                ))
            }
        };
        let _ = release_tx.send(());
        if owns_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(true) {
            return Err(
                "the holder whose worker is gone still believed it owned the lane -- the lane did not really drain"
                    .into(),
            );
        }
        drop(claimer);

        Ok("a holder stuck before running lost the lane as holder_stuck_before_running and knew it had, its claim parked at the head of the queue, while a running holder with nothing behind it kept the lane past the same bound, one running past the ceiling with a waiter behind it lost it and the lane drained to that waiter, and one whose worker was gone lost it as holder_worker_not_alive, was marked abandoned, and the lane drained".into())
    })();

    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real GmFairnessGuard: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_lane_fifo() -> anyhow::Result<()> {
    use agentplug_host::GmFairnessGuard;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const NAME: &str = "selfcheck-lane-fifo";
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lane-fifo-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    std::fs::create_dir_all(&root)?;

    const BODY_A: &str = "{\"SESSION_ID\":\"fifo-first\",\"paths\":[\"a\"]}";
    const BODY_B: &str = "{\"SESSION_ID\":\"fifo-later\",\"paths\":[\"b\"]}";

    let (held_tx, held_rx) = mpsc::channel::<bool>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder_root = root.clone();
    let holder = std::thread::spawn(move || {
        let guard = GmFairnessGuard::acquire_within(
            &holder_root,
            "git_commit",
            "{}",
            "holder",
            Duration::from_secs(5),
        );
        let _ = held_tx.send(guard.is_ok());
        let _ = release_rx.recv();
        drop(guard);
    });

    let outcome = (|| -> Result<String, String> {
        if !held_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the holder could not take the project git lane".into());
        }
        match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_A,
            "task-1",
            Duration::from_millis(400),
        ) {
            Ok(_) => return Err("waiter 1 took the lane while the holder still held it".into()),
            Err(report) => {
                if report.lane_position != Some(1) {
                    return Err(format!(
                        "waiter 1 starved at position {:?}, expected 1",
                        report.lane_position
                    ));
                }
                if report.lane_holder_task.as_deref() != Some("holder") {
                    return Err(format!(
                        "waiter 1 was refused without naming the holder (holder {:?})",
                        report.lane_holder_task
                    ));
                }
                if report.lane_holder_held_ms == 0 {
                    return Err(
                        "waiter 1 was refused without reporting how long the holder held the lane"
                            .into(),
                    );
                }
            }
        }
        match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_B,
            "task-2",
            Duration::from_millis(400),
        ) {
            Ok(_) => return Err("waiter 2 took the lane while the holder still held it".into()),
            Err(report) if report.lane_position != Some(2) => {
                return Err(format!(
                    "waiter 2 starved at position {:?}, expected 2 behind waiter 1",
                    report.lane_position
                ))
            }
            Err(_) => {}
        }
        match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_A,
            "task-1-redispatch",
            Duration::from_millis(400),
        ) {
            Ok(_) => {
                return Err("the re-dispatched waiter 1 took the lane while the holder held it".into())
            }
            Err(report) if report.lane_position != Some(1) => {
                return Err(format!(
                    "the re-dispatched waiter 1 lost its place (position {:?}, expected 1)",
                    report.lane_position
                ))
            }
            Err(_) => {}
        }
        let holds = agentplug_host::registry::lane_holds(&root);
        let named = holds.iter().any(|hold| {
            hold.lane == "git"
                && hold.task == "holder"
                && hold.verb == "git_commit"
                && hold.waiters >= 1
        });
        if !named {
            return Err(format!(
                "lane_holds did not name the git holder and its queued waiters (holds {:?})",
                holds
                    .iter()
                    .map(|hold| (hold.lane, hold.task.clone(), hold.waiters))
                    .collect::<Vec<_>>()
            ));
        }
        let _ = release_tx.send(());
        let first = match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_A,
            "task-1-redispatch-2",
            Duration::from_secs(3),
        ) {
            Ok(guard) => guard,
            Err(report) => {
                return Err(format!(
                    "waiter 1 never got the lane after the holder released it (still held by {:?})",
                    report.lane_holder_task
                ))
            }
        };
        match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_B,
            "task-2-redispatch",
            Duration::from_millis(400),
        ) {
            Ok(_) => return Err("waiter 2 took the lane while waiter 1 still held it".into()),
            Err(report) if report.lane_holder_task.as_deref() != Some("task-1-redispatch-2") => {
                return Err(format!(
                    "waiter 1 was not admitted before waiter 2 (lane held by {:?})",
                    report.lane_holder_task
                ))
            }
            Err(_) => {}
        }
        drop(first);
        match GmFairnessGuard::acquire_within(
            &root,
            "git_commit",
            BODY_B,
            "task-2-redispatch-2",
            Duration::from_secs(3),
        ) {
            Ok(_) => {}
            Err(report) => {
                return Err(format!(
                    "waiter 2 never got the lane after waiter 1 released it (still held by {:?})",
                    report.lane_holder_task
                ))
            }
        }

        let (held2_tx, held2_rx) = mpsc::channel::<bool>();
        let (release2_tx, release2_rx) = mpsc::channel::<()>();
        let holder2_root = root.clone();
        let holder2 = std::thread::spawn(move || {
            let guard = GmFairnessGuard::acquire_within(
                &holder2_root,
                "git_commit",
                "{}",
                "holder-2",
                Duration::from_secs(5),
            );
            let _ = held2_tx.send(guard.is_ok());
            let _ = release2_rx.recv();
            drop(guard);
        });
        if !held2_rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            return Err("the second holder could not take the project git lane".into());
        }
        let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for index in 1..=3 {
            let task = format!("claim-{index}");
            let body = format!("{{\"SESSION_ID\":\"fifo-claim-{index}\",\"paths\":[\"c{index}\"]}}");
            let waiter_root = root.clone();
            let order = Arc::clone(&order);
            handles.push(std::thread::spawn(move || {
                match GmFairnessGuard::acquire_within(
                    &waiter_root,
                    "git_commit",
                    &body,
                    &task,
                    Duration::from_secs(10),
                ) {
                    Ok(guard) => {
                        order.lock().unwrap_or_else(|e| e.into_inner()).push(task.clone());
                        drop(guard);
                    }
                    Err(_) => {
                        order
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(format!("{task}:starved"));
                    }
                }
            }));
            std::thread::sleep(Duration::from_millis(60));
        }
        std::thread::sleep(Duration::from_millis(20));
        let positions = ["claim-1", "claim-2", "claim-3"]
            .iter()
            .map(|task| {
                agentplug_host::registry::lane_queue_state(&root, Some("git"), task).lane_position
            })
            .collect::<Vec<_>>();
        if positions != vec![Some(1), Some(2), Some(3)] {
            return Err(format!(
                "the git lane did not rank its waiters by claim age (positions {positions:?})"
            ));
        }
        let _ = release2_tx.send(());
        let _ = holder2.join();
        for handle in handles {
            let _ = handle.join();
        }
        let admitted = order.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if admitted
            != vec![
                "claim-1".to_string(),
                "claim-2".to_string(),
                "claim-3".to_string(),
            ]
        {
            return Err(format!(
                "the git lane did not admit its waiters oldest-claim-first (order {admitted:?})"
            ));
        }

        Ok("a starved git dispatch kept its queue position across re-dispatch, a later arrival never overtook it, and the lane admitted oldest-claim-first with exactly one holder at a time".into())
    })();
    let _ = release_tx.send(());
    let _ = holder.join();

    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(detail) => {
            println!("[{NAME}] {detail}");
            println!("[{NAME}] witnessed live against the real GmFairnessGuard: PASS");
            println!("RESULT: PASS");
            Ok(())
        }
        Err(reason) => selfcheck_fail(NAME, &reason),
    }
}

fn selfcheck_inflight_cleanup() -> anyhow::Result<()> {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-inflight-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    let spool_dir = root.join(".gm").join("exec-spool");
    let out_dir = spool_dir.join("out");
    fs::create_dir_all(&out_dir)?;

    let project = ProjectPlugins::new(root.clone());
    let handle = project.dispatch_handle();
    let key: daemon::InFlightKey = (root.clone(), "verbX".to_string(), "taskY".to_string());
    daemon::in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key.clone(),
            daemon::InFlightHandle {
                detach: Arc::new(AtomicBool::new(false)),
            },
        );

    daemon::run_gm_dispatch_to_file(&root, &handle, "verbX", "taskY", "{}", &out_dir, 0, None);

    let entry_remains = daemon::in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .is_some();
    let out_written = out_dir.join("verbX-taskY.json").exists();
    println!("[selfcheck-inflight] entry_remains={entry_remains} out_written={out_written}");
    assert!(!entry_remains, "a completed dispatch must clear its own in-flight entry so the handoff/idle gates stop counting it");
    assert!(out_written, "the out file must still be written even when the dispatch itself errors (no registered plugin)");

    let _ = fs::remove_dir_all(&root);
    println!("[selfcheck-inflight] witnessed live against the real daemon dispatch path: PASS");
    Ok(())
}

fn selfcheck_idle_evict() -> anyhow::Result<()> {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const NAME: &str = "selfcheck-idle-evict";
    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-idle-evict-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    let spool_dir = root.join(".gm").join("exec-spool");
    fs::create_dir_all(&spool_dir)?;

    let mut project = ProjectPlugins::new(root.clone());
    project.last_active = Instant::now() - Duration::from_secs(3600);
    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    projects.insert(root.clone(), project);
    let evict_before = Instant::now() - Duration::from_millis(60_000);

    let quiet = daemon::idle_projects_to_evict(&projects, evict_before);
    let evicted_when_quiet = quiet.to_evict.iter().any(|p| p == &root);
    println!(
        "[selfcheck-idle-evict] past the idle window with no claim: evicted={evicted_when_quiet} deferred={}",
        quiet.deferred_live_claims
    );
    if !evicted_when_quiet {
        return selfcheck_fail(
            NAME,
            "a project past its idle window with no live claim must still be evicted",
        );
    }

    let key: daemon::InFlightKey = (root.clone(), "verbX".to_string(), "taskY".to_string());
    daemon::in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key.clone(),
            daemon::InFlightHandle {
                detach: Arc::new(AtomicBool::new(false)),
            },
        );
    let claimed = daemon::idle_projects_to_evict(&projects, evict_before);
    let evicted_while_claimed = claimed.to_evict.iter().any(|p| p == &root);
    println!(
        "[selfcheck-idle-evict] live in-flight admission claim: evicted={evicted_while_claimed} deferred={}",
        claimed.deferred_live_claims
    );
    daemon::in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if evicted_while_claimed {
        return selfcheck_fail(
            NAME,
            "a project holding a live admission claim must not be evicted",
        );
    }

    fs::write(
        spool_dir.join(".status.json"),
        format!(
            "{{\"busy_until\":{}}}",
            agentplug_host::now_ms() + 120_000
        ),
    )?;
    let beating = daemon::idle_projects_to_evict(&projects, evict_before);
    let evicted_while_heartbeating = beating.to_evict.iter().any(|p| p == &root);
    println!(
        "[selfcheck-idle-evict] live heartbeat promising work: evicted={evicted_while_heartbeating} deferred={}",
        beating.deferred_live_claims
    );
    let _ = fs::remove_file(spool_dir.join(".status.json"));
    if evicted_while_heartbeating {
        return selfcheck_fail(
            NAME,
            "a project whose heartbeat promises work until a future instant must not be evicted",
        );
    }

    let released = daemon::idle_projects_to_evict(&projects, evict_before);
    let evicted_after_release = released.to_evict.iter().any(|p| p == &root);
    println!(
        "[selfcheck-idle-evict] after the claim and heartbeat clear: evicted={evicted_after_release} deferred={}",
        released.deferred_live_claims
    );
    if !evicted_after_release {
        return selfcheck_fail(
            NAME,
            "eviction must resume once the claim and heartbeat clear",
        );
    }

    let _ = fs::remove_dir_all(&root);
    println!("[selfcheck-idle-evict] witnessed live against the real idle-eviction path: PASS");
    println!("RESULT: PASS");
    Ok(())
}

fn selfcheck_spool_claim() -> anyhow::Result<()> {
    use std::fs;

    let in_dir = std::env::temp_dir()
        .join(format!(
            "agentplug-selfcheck-claim-{}-{}",
            std::process::id(),
            agentplug_host::now_ms()
        ))
        .join("codesearch");
    fs::create_dir_all(&in_dir)?;
    let txt = in_dir.join("s-1.txt");

    fs::write(&txt, b"")?;
    let empty_claimed = daemon::claim_spool_request_in_place(&txt).is_some();

    fs::write(&txt, br#"{"session_id":"s","query":"x"}"#)?;
    let fresh_claimed = daemon::claim_spool_request_in_place(&txt).is_some();

    println!("[selfcheck-spool-claim] empty_claimed={empty_claimed} fresh_claimed={fresh_claimed}");
    assert!(!empty_claimed, "an empty in-file is the first half of a shell redirect, never a request: claiming it dispatches a torn body");
    assert!(fresh_claimed, "a non-empty in-file is claimable immediately: publishers rename a complete sibling into place");

    let _ = fs::remove_dir_all(in_dir.parent().unwrap_or(&in_dir));
    println!("[selfcheck-spool-claim] witnessed live against the real claim predicate: PASS");
    Ok(())
}
// Reads the release channel for the runner, every loaded plugin and the skill manifest, without staging,
// installing or writing anything except the skill manifest cache the daemon already keeps.
fn selfcheck_update_probes() -> anyhow::Result<()> {
    const NAME: &str = "selfcheck-update-probes";
    let mut problems: Vec<String> = Vec::new();
    match download::fetch_latest_runner_version() {
        Ok(Some(latest)) => println!("runner: running {}, latest release {latest}", env!("CARGO_PKG_VERSION")),
        Ok(None) => problems.push("runner: no latest release resolved".to_string()),
        Err(e) => problems.push(format!("runner: probe failed: {e:#}")),
    }
    for plugin in ["gm", "bert", "libsql", "treesitter", "crux"] {
        let installed = download::installed_plugin_version(plugin).unwrap_or_else(|| "none".to_string());
        match download::probe_latest_plugin_release(plugin) {
            Ok(Some(release)) => {
                let digest: String = release.sha256.chars().take(12).collect();
                println!("plugin {plugin}: installed {installed}, latest release {} (sha {digest})", release.version);
            }
            Ok(None) => problems.push(format!("plugin {plugin}: no latest release resolved")),
            Err(e) => problems.push(format!("plugin {plugin}: probe failed: {e:#}")),
        }
    }
    match download::skill_manifest_status() {
        Ok(status) => println!("skill manifest: {status}"),
        Err(e) => problems.push(format!("skill manifest: {e:#}")),
    }
    if problems.is_empty() {
        println!("RESULT: PASS");
        Ok(())
    } else {
        selfcheck_fail(NAME, &problems.join("; "))
    }
}

fn selfcheck_browser_lease() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const NAME: &str = "[selfcheck-browser-lease]";
    const HOLD: &str = "AGENTPLUG_SELFCHECK_LEASE_HOLD";
    const ROOT: &str = "AGENTPLUG_SELFCHECK_LEASE_ROOT";

    if std::env::var_os(HOLD).is_some() {
        let root = std::path::PathBuf::from(std::env::var(ROOT).unwrap_or_default());
        agentplug_host::claim_session(&root, "selfcheck-holder", agentplug_host::TTL_MS, 30_000)
            .map_err(|e| anyhow::anyhow!("{NAME} holder could not claim its own session: {e}"))?;
        println!("{NAME} holder claimed pid={}", std::process::id());
        let stop = agentplug_host::session_dir(&root).join("stop");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !stop.exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
        agentplug_host::release_session(&root);
        println!("{NAME} holder released");
        return Ok(());
    }

    let root = std::env::temp_dir().join(format!(
        "agentplug-selfcheck-lease-{}-{}",
        std::process::id(),
        agentplug_host::now_ms()
    ));
    let session = agentplug_host::session_dir(&root);
    std::fs::create_dir_all(&session)?;

    let exe = std::env::current_exe()?;
    let mut child = Command::new(&exe)
        .arg("selfcheck-browser-lease")
        .env(HOLD, "1")
        .env(ROOT, &root)
        .stdin(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = false;
    while Instant::now() < deadline {
        let state = agentplug_host::lease_status(&root);
        if state["held"].as_bool() == Some(true) && state["live"].as_bool() == Some(true) {
            seen = true;
            println!(
                "{NAME} holder live: owner={} pid={} heartbeat_age_ms={}",
                state["owner"], state["pid"], state["heartbeat_age_ms"]
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen, "{NAME} the first process must publish a live session claim");

    let second =
        agentplug_host::claim_session(&root, "selfcheck-waiter", agentplug_host::TTL_MS, 1_500);
    let err = match second {
        Ok(_) => panic!(
            "{NAME} a live holder must refuse a second claim: two processes would launch two browsers"
        ),
        Err(e) => e,
    };
    println!("{NAME} second process refused: {err}");
    assert!(
        err.contains("selfcheck-holder"),
        "{NAME} the refusal must name the holder: {err}"
    );
    assert!(
        err.contains("no browser was launched"),
        "{NAME} the refusal must say no browser was launched: {err}"
    );

    std::fs::write(session.join("stop"), b"")?;
    let exited = child.wait()?;
    assert!(
        exited.success(),
        "{NAME} the holder must exit cleanly after its release"
    );

    let released = agentplug_host::lease_status(&root);
    println!("{NAME} after release held={}", released["held"]);
    assert!(
        released["held"].as_bool() == Some(false),
        "{NAME} releasing the holder must drop its claim"
    );

    agentplug_host::claim_session(&root, "selfcheck-waiter", agentplug_host::TTL_MS, 1_500)
        .map_err(|e| anyhow::anyhow!("{NAME} the next process must claim after a release: {e}"))?;
    agentplug_host::release_session(&root);

    let stale_epoch = agentplug_host::now_ms().saturating_sub(10 * 60_000);
    std::fs::write(
        agentplug_host::owner_file(&root),
        format!(
            "{{\"owner\":\"dead-holder\",\"pid\":1,\"browser_pid\":1,\"port\":0,\"startedAt\":{stale_epoch},\"heartbeatAt\":{stale_epoch},\"ttlMs\":{}}}",
            agentplug_host::TTL_MS
        ),
    )?;
    assert!(
        agentplug_host::lease_status(&root)["live"].as_bool() == Some(false),
        "{NAME} a heartbeat older than the TTL is a dead holder, not a live one"
    );
    agentplug_host::claim_session(&root, "selfcheck-after-stale", agentplug_host::TTL_MS, 2_000)
        .map_err(|e| anyhow::anyhow!("{NAME} a dead holder must be taken over, not waited for: {e}"))?;
    agentplug_host::release_session(&root);

    let _ = std::fs::remove_dir_all(&root);
    println!("{NAME} two real processes against the real claim file: RESULT: PASS");
    Ok(())
}

fn write_standalone_status(status_path: &std::path::Path, busy_until: Option<u64>) {
    use std::fs;
    let mut payload = match fs::read_to_string(status_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => serde_json::json!({}),
    };
    payload["pid"] = serde_json::json!(std::process::id());
    payload["ts"] = serde_json::json!(agentplug_host::now_ms());
    payload["runtime"] = serde_json::json!("agentplug-runner-standalone");
    payload["daemon"] = serde_json::json!(false);
    payload["shared_process"] = serde_json::json!(false);
    if let Some(busy_until) = busy_until {
        payload["busy_until"] = serde_json::json!(busy_until);
    } else {
        payload.as_object_mut().map(|m| m.remove("busy_until"));
    }
    let _ = fs::write(status_path, payload.to_string());
}

const STANDALONE_BUSY_HEARTBEAT_MS: u64 = 5_000;
const STANDALONE_BUSY_EXTEND_MS: u64 = 20_000;

fn spawn_standalone_busy_ticker(
    status_path: std::path::PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            write_standalone_status(
                &status_path,
                Some(agentplug_host::now_ms() + STANDALONE_BUSY_EXTEND_MS),
            );
            std::thread::sleep(std::time::Duration::from_millis(
                STANDALONE_BUSY_HEARTBEAT_MS,
            ));
        }
    })
}

fn clear_standalone_status(status_path: &std::path::Path) {
    use std::fs;
    let Some(serde_json::Value::Object(mut map)) = fs::read_to_string(status_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return;
    };
    map.remove("runtime");
    map.remove("daemon");
    map.remove("shared_process");
    map.insert(
        "ts".to_string(),
        serde_json::json!(agentplug_host::now_ms()),
    );
    let _ = fs::write(status_path, serde_json::Value::Object(map).to_string());
}

fn run_spool_watcher_single_process(
    project: &mut ProjectPlugins,
    spool_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use std::fs;
    use std::time::Duration;

    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    fs::create_dir_all(&in_dir)?;
    fs::create_dir_all(&out_dir)?;
    let status_path = spool_dir.join(".status.json");

    if !daemon::claim_standalone_watcher_slot(spool_dir) {
        eprintln!(
            "[agentplug] another standalone watcher is already live for {} -- exiting rather than sweeping the claims it is serving",
            spool_dir.display()
        );
        return Ok(());
    }

    loop {
        if daemon::shared_daemon_is_serving() {
            clear_standalone_status(&status_path);
            daemon::release_standalone_watcher_slot(spool_dir);
            eprintln!(
                "[agentplug] shared daemon is serving again -- standalone watcher for {} exiting between dispatches, leaving every unclaimed request in the spool for it",
                spool_dir.display()
            );
            return Ok(());
        }

        write_standalone_status(&status_path, None);

        let mut work_done = false;
        if let Ok(verb_dirs) = fs::read_dir(&in_dir) {
            for verb_entry in verb_dirs.flatten() {
                if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let verb = verb_entry.file_name().to_string_lossy().into_owned();
                let verb_dir = verb_entry.path();
                let Ok(files) = fs::read_dir(&verb_dir) else {
                    continue;
                };
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                        continue;
                    }
                    let Some(claim_path) = daemon::claim_spool_request_in_place(&path) else {
                        continue;
                    };
                    let Ok(body) = fs::read_to_string(&claim_path) else {
                        let _ = fs::rename(&claim_path, &path);
                        continue;
                    };
                    if body.trim().is_empty() {
                        let _ = fs::rename(&claim_path, &path);
                        continue;
                    }
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let _dispatch_origin_scope =
                        agentplug_host::enter_dispatch_origin_scope(&stem, None);

                    let busy_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let ticker =
                        spawn_standalone_busy_ticker(status_path.clone(), busy_stop.clone());
                    let result = project.dispatch("gm", &verb, &body).unwrap_or_else(|e| {
                        serde_json::json!({"ok": false, "verb": verb, "error": e.to_string()})
                            .to_string()
                    });
                    busy_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = ticker.join();

                    let out_confirmed =
                        daemon::write_spool_out_confirmed(&out_dir, &verb, &stem, &result);
                    if out_confirmed {
                        let _ = fs::remove_file(&claim_path);
                    } else {
                        eprintln!(
                            "[agentplug] standalone: out-file write for {verb}/{stem} did not confirm -- leaving the claim in place for the orphan sweep to retry rather than deleting an unanswered request"
                        );
                    }
                    work_done = true;
                }
            }
        }
        if !work_done {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}
