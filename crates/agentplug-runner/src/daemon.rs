use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use wasmtime::{Engine, Module, Trap};

use agentplug_host::{build_engine, install_dir, now_ms, read_project_plugin_list, DispatchHandle, GmFairnessGuard, ProjectPlugins};

use crate::download::{ensure_plugin_installed, installed_plugin_version, installed_runner_version, is_recognized_release_semver, record_runner_version};

fn registry_path() -> PathBuf {
    install_dir().join("daemon-registry.txt")
}

fn cwd_is_inside_a_spool_tree(cwd: &Path) -> bool {
    cwd.components().any(|c| c.as_os_str() == ".gm")
        && cwd.to_string_lossy().replace('\\', "/").contains("/.gm/exec-spool")
}

pub fn register_project(cwd: &Path) -> anyhow::Result<()> {
    if cwd_is_inside_a_spool_tree(cwd) {
        anyhow::bail!(
            "refusing to register {} as a project root -- its own path is already inside a .gm/exec-spool tree, which means this is spool runtime state (in/out/status files), not a genuine project directory. Launch the spool from the actual project root instead.",
            cwd.display()
        );
    }
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let cwd_str = cwd.to_string_lossy().to_string();

    let mut live: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for line in existing.lines() {
        let entry = line.trim();
        if entry.is_empty() || live.iter().any(|e| e == entry) {
            continue;
        }
        if entry == cwd_str || Path::new(entry).exists() {
            live.push(entry.to_string());
        } else {
            dropped += 1;
        }
    }

    let already_present = live.iter().any(|e| e == &cwd_str);
    if already_present && dropped == 0 {
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

fn describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(e: &anyhow::Error) -> String {
    match e.downcast_ref::<Trap>() {
        Some(trap) => format!("[wasm trap: {trap}] {e:#}"),
        None => format!("{e:#}"),
    }
}

pub(crate) fn read_registry() -> Vec<PathBuf> {
    fs::read_to_string(registry_path())
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

fn host_available_parallelism() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(4)
}

#[derive(serde::Deserialize, Clone)]
struct DaemonConfig {
    #[serde(default)]
    registry_poll_interval_secs: Option<u64>,
    #[serde(default)]
    heartbeat_interval_secs: Option<u64>,
    #[serde(default)]
    plugin_update_poll_interval_secs: Option<u64>,
    /// Per-plugin poll-CHECK cadence overrides (e.g. `{"bert": 3600}`),
    /// keyed by plugin name -- unlisted plugins use
    /// `plugin_update_poll_interval_secs`. Independent of per-plugin RELOAD
    /// independence (already unconditional: `refresh_plugin_if_stale` only
    /// reloads a plugin whose own content hash actually changed) -- this
    /// field controls how often the poll-check itself fires per plugin, not
    /// whether a check that finds nothing new still reloads something.
    #[serde(default)]
    plugin_update_poll_interval_secs_by_name: std::collections::HashMap<String, u64>,
    #[serde(default)]
    runner_update_poll_interval_secs: Option<u64>,
    #[serde(default)]
    instruction_source_poll_interval_secs: Option<u64>,
    #[serde(default)]
    max_concurrent_projects: Option<usize>,
    #[serde(default)]
    gm_concurrency: Option<usize>,
    #[serde(default)]
    side_plugin_concurrency: Option<usize>,
    #[serde(default)]
    shared_store_recycle_private_mb: Option<u64>,
    #[serde(default)]
    shared_store_recycle_dispatches: Option<u64>,
    #[serde(default)]
    project_idle_evict_secs: Option<u64>,
    #[serde(default)]
    shared_plugin_release_idle_secs: Option<u64>,
}

// max_concurrent_projects/gm_concurrency/side_plugin_concurrency/
// shared_store_recycle_private_mb/shared_store_recycle_dispatches are
// deliberately absent here: leaving them unset lets DaemonConfig's accessors
// derive a default from this machine's actual available_parallelism() (the
// first three directly, the last two via gm_concurrency()'s pool size) at
// every boot. Baking a literal number into this scaffold (as used to happen)
// would freeze that number into daemon-config.json on the very first run and
// make every future boot re-read the same static value forever, regardless
// of how many cores the host actually has -- an operator who wants a fixed
// value can still add these keys back by hand.
const DAEMON_CONFIG_EXAMPLE: &str = r#"{
  "registry_poll_interval_secs": 5,
  "heartbeat_interval_secs": 10,
  "plugin_update_poll_interval_secs": 600,
  "plugin_update_poll_interval_secs_by_name": {},
  "runner_update_poll_interval_secs": 3600,
  "instruction_source_poll_interval_secs": 600
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
                max_concurrent_projects: None,
                gm_concurrency: None,
                side_plugin_concurrency: None,
                shared_store_recycle_private_mb: None,
                shared_store_recycle_dispatches: None,
                project_idle_evict_secs: None,
                shared_plugin_release_idle_secs: None,
            }
    }
    fn registry_poll_interval(&self) -> Duration { Duration::from_secs(self.registry_poll_interval_secs.unwrap_or(5)) }
    fn heartbeat_interval(&self) -> Duration { Duration::from_secs(self.heartbeat_interval_secs.unwrap_or(10)) }
    fn plugin_update_poll_interval(&self) -> Duration { Duration::from_secs(self.plugin_update_poll_interval_secs.unwrap_or(600)) }
    fn plugin_update_poll_interval_for(&self, plugin_name: &str) -> Duration {
        match self.plugin_update_poll_interval_secs_by_name.get(plugin_name) {
            Some(secs) => Duration::from_secs(*secs),
            None => self.plugin_update_poll_interval(),
        }
    }
    // Runner binaries update least often by design: they are the sole spool
    // loader and every project's daemon depends on one staying stable, so a
    // longer default poll cadence than plugin updates (600s) is deliberate,
    // not an oversight -- 1hr default, still fully overridable.
    fn runner_update_poll_interval(&self) -> Duration { Duration::from_secs(self.runner_update_poll_interval_secs.unwrap_or(3600)) }
    // Explicit interval for the per-project .gm/instructions/source.json sync,
    // previously coupled only incidentally to plugin_update_poll_interval's
    // Duration value (same number, not the same timer) -- an independent key
    // so tuning one cadence never silently retunes the other.
    fn instruction_source_poll_interval(&self) -> Duration { Duration::from_secs(self.instruction_source_poll_interval_secs.unwrap_or(600)) }
    fn max_concurrent_projects(&self) -> usize { self.max_concurrent_projects.unwrap_or_else(host_available_parallelism).max(1) }
    // gm_concurrency still legitimately scales with core count -- it feeds
    // shared_store_recycle_dispatches' scaling and (via max_concurrent_
    // projects) the worker-THREAD pool that dispatches across different
    // PROJECTS in parallel, which is genuine, independent CPU-bound work
    // that benefits from oversubscription. It no longer sizes the gm
    // plugin's own Store pool -- see gm_pool_size below, same reasoning as
    // side_plugin_concurrency: one project's gm dispatch and another
    // project's gm dispatch are serialized work against the SAME plugin
    // binary's semantics, not independent CPU-bound work multiplied by
    // core count, and each pool slot is its own memory-costly Store copy.
    fn gm_concurrency(&self) -> usize { self.gm_concurrency.unwrap_or_else(|| self.max_concurrent_projects()).max(1) }
    // Every plugin type gets exactly ONE hot, warm-resident Store instance,
    // serializing calls through it -- not one-per-worker-slot. A single
    // instance stays loaded between calls (no repeated cold-reload cost, so
    // per-call latency stays fast) while never duplicating a plugin's
    // resident memory across multiple concurrent copies; throughput under
    // genuinely heavy concurrent load is serial rather than parallel, which
    // is the intended tradeoff -- these plugins are memory-costly-but-fast
    // (per-call latency live-measured at 600ms-1.4s), not CPU-bound work
    // that benefits from N-way oversubscription.
    fn gm_pool_size(&self) -> usize { 1 }
    // Side plugins (bert/libsql/treesitter) used to default to half the
    // host's cores (a throughput optimization: avoid serializing every call
    // behind one slot on a many-core host) -- but each filled slot holds its
    // OWN independent copy of that plugin's instantiated Store, and for
    // bert specifically that means its own copy of the loaded
    // BAAI/bge-small-en-v1.5 embedding model's real weights in linear
    // memory, not just cheap dispatch state. On a 16-core host this derived
    // 8 slots per side plugin -- live-witnessed: bert alone filling even 2-3
    // of its 8 slots under real (not even unusually heavy) concurrent use
    // pushed process memory from a ~300MB single-slot baseline past 1.7GB.
    // Same fix as gm_pool_size above and for the same reason: one hot,
    // warm-resident instance per plugin, serialized -- stays loaded between
    // calls (fast per-call latency, live-measured 600ms-1.4s, no repeated
    // cold-reload), never duplicated across concurrent slots. Throughput
    // under heavy concurrent load is serial, which is the accepted tradeoff
    // for a memory-costly-but-fast plugin instead of a CPU-bound one.
    fn side_plugin_concurrency(&self) -> usize {
        self.side_plugin_concurrency.unwrap_or(1).max(1)
    }
    // The recycle threshold used to scale with gm_concurrency() (400MB per
    // worker slot, uncapped) on the theory that "how much is normal before
    // recycling" should track how many concurrent Store slots exist -- on a
    // 16-core host that derived a 6400MB ceiling, well past what real
    // system-wide memory pressure can tolerate alongside everything else
    // running on the same machine (live-witnessed 2026-08-24: this daemon's
    // own restart churn correlating with the host down to ~3GB free of
    // 15.6GB total).
    //
    // The 1.7GB "steady state" this default was originally calibrated
    // against (an earlier commit on the same day set this to 2048MB with
    // headroom above that number) turned out to be itself a symptom, not a
    // legitimate baseline: side_plugin_concurrency/gm_pool_size (see above)
    // defaulted to core-count-scaled pool sizes (8 slots for bert/
    // treesitter on this 16-core host, each an independent full copy of
    // that plugin's Store -- for bert specifically, its own copy of the
    // loaded BAAI/bge-small-en-v1.5 model's real weights), so what looked
    // like "real multi-project load" was substantially N redundant copies
    // of the same plugin state. With pool sizes fixed to one hot instance
    // per plugin, live-witnessed real single-slot baseline is ~300MB
    // (matches this daemon's own documented normal-operation memory). Set
    // with real headroom above that corrected baseline, still a small
    // fraction of what either the old core-scaled formula (6400MB) or the
    // now-corrected-away 1.7GB "steady state" would have implied -- still
    // overridable via shared_store_recycle_private_mb for an operator whose
    // own host's real working set differs.
    fn shared_store_recycle_private_bytes(&self) -> u64 {
        const DEFAULT_MB: u64 = 768;
        self.shared_store_recycle_private_mb.unwrap_or(DEFAULT_MB).max(256) * 1024 * 1024
    }
    fn shared_store_recycle_dispatches(&self) -> u64 {
        let default = 500u64.saturating_mul(self.gm_concurrency() as u64).max(100);
        self.shared_store_recycle_dispatches.unwrap_or(default).max(1)
    }
    // The real blowup mechanism, not the recycle-threshold gate above: the
    // shared plugins (bert/treesitter/gm) are the ones that gate polices, but
    // each project's OWN non-shared plugins (libsql/oxibrowser/crux -- see
    // agentplug-host's is_stateless_shared_plugin, which does NOT include
    // these three) get a dedicated Store per project, held alive by
    // agentplug_host::PLUGIN_IDLE_EVICT_MS (30 minutes, a hard-coded constant
    // in a different crate this daemon calls into, not previously
    // configurable). register_project() only ever drops a project once its
    // path stops existing on disk -- never on inactivity -- so a machine that
    // has served 100+ projects across many sessions keeps ALL of them
    // registered forever, and any project touched even once in the last 30
    // minutes stays fully warm with its own per-project Store set live. On a
    // shared multi-session machine (live-witnessed: 103 registered projects,
    // nearly all warm simultaneously, 1.7GB real process memory) that 30-
    // minute window is generous enough that the warm set rarely shrinks at
    // all under continuous multi-session use -- this is the actual growth
    // mechanism the byte-recycle gate above can only clean up AFTER the fact,
    // never prevent. Making the window configurable (in agentplug-runner, the
    // only caller of the eviction check, since agentplug-host's own constant
    // has no config plumbing to reach) is the real fix -- an operator who has
    // actually measured their own fleet's idle-time distribution can tune it
    // down. The DEFAULT stays at the original, long-proven-safe 30 minutes:
    // adversarial review of an earlier draft (300s default) correctly found
    // that number was an unvalidated guess with no idle-time-distribution
    // evidence behind it (unlike shared_store_recycle_private_bytes's 2048MB,
    // which cites a real observed steady-state), and that a session doing
    // normal human-in-the-loop interactive dispatches every 60-90s would
    // never register the win a short window is meant to provide anyway
    // (active-use polling keeps last_active fresh far more often than any
    // reasonable window) -- so a lower default trades a real, proven-safe
    // baseline for an unproven one with no offsetting benefit demonstrated
    // for the actual workload. The floor is likewise raised from 30 SECONDS
    // (a genuine footgun: any interactive cadence slower than the floor
    // evicts and cold-reloads on literally every dispatch, strictly worse
    // than no fix at all) to 60 seconds, still enforced, but no longer able
    // to silently produce worse-than-baseline behavior from a single-digit
    // misconfiguration.
    fn project_idle_evict_ms(&self) -> u64 {
        const DEFAULT_SECS: u64 = 30 * 60; // unchanged from the prior hard-coded constant
        self.project_idle_evict_secs.unwrap_or(DEFAULT_SECS).max(60) * 1000
    }
    // The prior 120s (2min) hardcoded default cold-dropped the hot bert/
    // treesitter/gm pool slots on every ordinary lull between bursts of
    // dispatch activity -- live-witnessed firing 10+ times across one
    // session with 103 registered projects, each drop costing a real
    // wasm-instantiate + first-forward-pass warmup (bert alone measured at
    // 5.5s for one embed call right after a reload) stacked on top of
    // whatever queue wait already existed, intermittently exceeding even a
    // widened client-side timeout. That 120s figure predates the pool_size=1
    // "always keep exactly one hot instance" design (an earlier fix, when
    // pools scaled with core count) and was never revisited when the design
    // changed to deliberately favor latency over idle memory reclaim -- this
    // is the SAME architectural conflict project_idle_evict_ms's own history
    // above already fixed once for per-project non-shared plugins, now fixed
    // for the shared bert/treesitter/gm pool too. The memory-pressure
    // recycle gate (shared_store_recycle_private_bytes/_dispatches) remains
    // the real safety valve against unbounded wasm linear-memory growth;
    // this time-based release only needs to matter for a GENUINELY long
    // idle stretch, not an ordinary multi-minute gap between turns.
    fn shared_plugin_release_idle_ms(&self) -> u64 {
        // 30 minutes, matching project_idle_evict_ms's default -- not a
        // borrowed number left unexamined, but independently right for the
        // same reason that mechanism's default is right: this daemon has NO
        // idle-time-distribution evidence for the shared-plugin-specific
        // case beyond the single 120s-was-too-short data point (10+ evictions
        // in one session), which only bounds the problem from below, not
        // above. Absent a second real number to anchor a different default,
        // matching the one mechanism in this file that DOES have a validated
        // default (project_idle_evict_ms: adversarial review already rejected
        // a lower unproven guess there for the identical reason -- no
        // offsetting benefit demonstrated for the actual workload) is the
        // defensible choice, not an unexamined copy.
        const DEFAULT_SECS: u64 = 30 * 60;
        // A 60s floor (project_idle_evict_ms's own floor, a DIFFERENT
        // mechanism -- that one evicts a whole idle PROJECT, gated on
        // per-project last-active time; this one releases a plugin SHARED
        // across every active project, so "quiet" here means the entire
        // daemon saw zero dispatch work across ALL 100+ registered projects,
        // a materially rarer condition than any single project going quiet.
        // A 60s floor would let this specific mechanism react to an ordinary
        // cross-project lull the sibling mechanism's own 60s floor was never
        // exposed to) -- re-derived here instead: 5 minutes is comfortably
        // longer than a normal pause between bursts of dispatch activity
        // across the WHOLE daemon, so a value below it can only be a
        // deliberate choice to prioritize idle memory reclaim over latency,
        // not an accidental near-zero misconfiguration. Not starvable by
        // bursty traffic alone: shared_store_recycle_dispatches (checked
        // independent of idle state, every loop tick) still reclaims on
        // cumulative dispatch count even if the daemon is never quiet long
        // enough for THIS idle-based path to fire -- the two mechanisms
        // cover disjoint conditions (quiet-but-not-yet-pressured vs
        // busy-and-pressured), not the same one twice.
        const MIN_SECS: u64 = 5 * 60;
        self.shared_plugin_release_idle_secs.unwrap_or(DEFAULT_SECS).max(MIN_SECS) * 1000
    }
}

fn shared_store_recycle_reason_independent_of_daemon_idle_state(cfg: &DaemonConfig) -> Option<String> {
    let dispatches = agentplug_host::shared_dispatches_since_release();
    if let Some(private_bytes) = agentplug_host::process_private_bytes_tracking_retained_wasm_peak_unlike_working_set() {
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
        return Some(format!("dispatch budget: {dispatches} shared dispatches >= {dispatch_limit} limit"));
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

fn read_owner_pid() -> Option<u64> {
    fs::read_to_string(daemon_owner_path()).ok().and_then(|s| s.trim().parse::<u64>().ok())
}

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

    // The owner looks stale/dead -- but so can any number of concurrent
    // challengers see the exact same thing at once (observed live 2026-08-24:
    // 3 daemons launched within ~80ms of a fresh Windows boot, each reading a
    // pre-reboot owner pid as dead, each unconditionally overwriting
    // daemon-owner.lock in turn with no re-check against what the OTHER
    // challengers just wrote -- authority volleys between them and the
    // heartbeat ticker's own periodic recheck never finds a stable winner to
    // converge on, so none of them ever self-exits). Two things close this:
    // (1) a deterministic tie-break -- lowest pid wins a multi-challenger
    // race -- so every challenger computes the SAME winner from the same
    // observed candidate set instead of whoever's rename lands last, and
    // (2) only overwrite the file if it still names a stale/dead pid AT THE
    // MOMENT of the write, immediately before the rename, shrinking the
    // read-then-write window to as close to zero as fs operations allow.
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
            // A lower-pid'd live challenger is also contending this same
            // stale-owner window -- defer to it rather than racing a rename;
            // it will either win outright or itself defer further up the
            // chain, so exactly one process in any concurrent group ends up
            // writing, instead of every process taking a turn.
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
    // Hold the spawn lock across spawn_detached_daemon() AND the freshness wait,
    // not just the spawn call itself. Releasing it right after spawn_detached_daemon()
    // returns (its previous position) opened a real TOCTOU window: a concurrent
    // ensure_daemon_running() caller could see the lock file gone, is_daemon_fresh()
    // still false (the just-spawned process hasn't written its first heartbeat to
    // daemon-status.json yet -- process startup + wasm load is real wall-clock time),
    // re-acquire the now-free lock, and spawn a SECOND daemon. Observed live: 4
    // separate agentplug-runner.exe processes running simultaneously from a handful
    // of `bun x gm-plugkit@latest spool` calls a few minutes apart, each missing the
    // freshness window the previous spawn's daemon hadn't cleared yet. Keeping the
    // lock held through the same wait-for-fresh loop this function already runs
    // (previously only for the "someone else is spawning" branch above) closes the
    // window: any concurrent caller now blocks on the lock file instead of racing
    // past a released-but-not-yet-fresh gap.
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
    let Ok(raw) = fs::read_to_string(daemon_status_path()) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    let Some(ts) = v.get("ts").and_then(|t| t.as_u64()) else { return false };
    if now_ms().saturating_sub(ts) >= DAEMON_STALE_MS { return false; }
    let Some(pid) = v.get("pid").and_then(|p| p.as_u64()) else { return false };
    pid_is_alive(pid)
}

#[cfg(windows)]
fn pid_is_alive(pid: u64) -> bool {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().next().map(|l| l.contains(',')).unwrap_or(false)
        }
        Err(_) => true,
    }
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
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
    if fs::metadata(&path).map(|m| m.len() > DAEMON_LOG_MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_extension("log.prev"));
    }
    fs::OpenOptions::new().create(true).append(true).open(&path).ok()
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
fn default_branch() -> String { "main".to_string() }

fn instruction_source_config_path(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions").join("source.json")
}

fn instruction_source_cache_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("instructions-source-cache")
}

fn run_git_bounded(args: &[&str]) -> anyhow::Result<std::process::Output> {
    use wait_timeout::ChildExt;
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let timeout_ms = agentplug_host::git_subprocess_timeout_ms();
    match child.wait_timeout(Duration::from_millis(timeout_ms))? {
        Some(status) => {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut o) = child.stdout.take() { let _ = std::io::Read::read_to_end(&mut o, &mut stdout); }
            if let Some(mut e) = child.stderr.take() { let _ = std::io::Read::read_to_end(&mut e, &mut stderr); }
            Ok(std::process::Output { status, stdout, stderr })
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
    let Ok(raw) = fs::read_to_string(&config_path) else { return Ok(()) };
    let Ok(cfg) = serde_json::from_str::<InstructionSourceConfig>(&raw) else {
        eprintln!("[agentplug daemon] {} exists but does not parse as {{repo, branch?, path?}} -- ignoring", config_path.display());
        return Ok(());
    };
    let cache_dir = instruction_source_cache_dir(root);
    let cache_dir_str = cache_dir.to_string_lossy().into_owned();
    let git_dir_marker = cache_dir.join(".git");
    if !git_dir_marker.exists() {
        fs::create_dir_all(root.join(".gm"))?;
        let output = run_git_bounded(&["clone", "--depth", "1", "--branch", &cfg.branch, &cfg.repo, &cache_dir_str])?;
        if !output.status.success() {
            anyhow::bail!("git clone of {} (branch {}) failed", cfg.repo, cfg.branch);
        }
        eprintln!("[agentplug daemon] cloned instruction source {} (branch {}) for {}", cfg.repo, cfg.branch, root.display());
        return Ok(());
    }
    let fetch = run_git_bounded(&["-C", &cache_dir_str, "fetch", "--depth", "1", "origin", &cfg.branch])?;
    if !fetch.status.success() {
        anyhow::bail!("git fetch of {} (branch {}) failed", cfg.repo, cfg.branch);
    }
    let reset_target = format!("origin/{}", cfg.branch);
    let reset = run_git_bounded(&["-C", &cache_dir_str, "reset", "--hard", &reset_target])?;
    if !reset.status.success() {
        anyhow::bail!("git reset of instruction source cache for {} failed", root.display());
    }
    Ok(())
}

fn staged_binary_self_check(staged_exe: &Path, expected_version: &str) -> bool {
    let output = std::process::Command::new(staged_exe).arg("--version").output();
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
                staged_exe.display(), out.status
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
    if !staged_binary_self_check(staged_exe, version) {
        let _ = fs::remove_file(staged_exe);
        record_handoff_attempt(Some(format!("staged_binary_self_check failed for {version}, staged exe removed")));
        return false;
    }
    let ready_path = takeover_ready_path();
    let _ = fs::remove_file(&ready_path);
    if let Err(e) = spawn_detached(staged_exe, &["takeover", version]) {
        record_handoff_attempt(Some(format!("spawn_detached of staged {version} failed: {e}")));
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(raw) = fs::read_to_string(&ready_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v.get("version").and_then(|x| x.as_str()) == Some(version) {
                    eprintln!("[agentplug daemon] new version {version} confirmed ready -- releasing ownership for handoff");
                    record_handoff_attempt(None);
                    release_ownership_for_handoff();
                    return true;
                }
            }
        }
    }
    eprintln!("[agentplug daemon] self-update to {version} did not confirm ready in time -- staying on current version, will retry next poll");
    record_handoff_attempt(Some(format!("staged {version} did not write a matching readiness marker within 10s")));
    false
}

fn release_ownership_for_handoff() {
    let my_pid = std::process::id() as u64;
    if read_owner_pid() == Some(my_pid) {
        let _ = fs::remove_file(daemon_owner_path());
    }
}

fn promote_staged_exe_to_canonical(version: &str) -> bool {
    let Some(canonical) = canonical_runner_exe_path() else { return false };
    let Ok(staged) = std::env::current_exe() else { return false };
    if staged == canonical {
        return false;
    }
    let prev = canonical.with_extension(
        canonical.extension().map(|e| format!("{}.prev", e.to_string_lossy())).unwrap_or_else(|| "prev".to_string()),
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
    match fs::copy(&staged, &canonical) {
        Ok(_) => {
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&canonical) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    let _ = fs::set_permissions(&canonical, perms);
                }
            }
            record_completed_runner_swap(version);
            eprintln!("[agentplug daemon] takeover: promoted {version} onto canonical exe path {} (previous version kept at {})", canonical.display(), prev.display());
            true
        }
        Err(e) => {
            eprintln!(
                "[agentplug daemon] takeover: failed to copy staged exe onto canonical path {}: {e} -- restoring previous version at canonical path",
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
        Some(log) => { cmd.stderr(std::process::Stdio::from(log)); }
        None => { cmd.stderr(std::process::Stdio::null()); }
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
    for _ in 0..480 {
        if read_owner_pid().is_none() && claim_ownership() {
            record_runner_version(version)?;
            crate::download::clear_all_known_bad_version_markers();
            let promoted = promote_staged_exe_to_canonical(version);
            if promoted {
                if let Some(canonical) = canonical_runner_exe_path() {
                    // This process is still bound to the staged `.new` executable image for its
                    // whole lifetime (Windows keeps a live process's own backing file locked), so
                    // continuing in-process would leave that file permanently un-removable and
                    // collide with every future self-update's staging path. Re-exec from the
                    // freshly-promoted canonical path and let this process exit instead.
                    release_ownership_for_handoff();
                    reexec_from_canonical_and_exit(&canonical);
                }
            }
            eprintln!("[agentplug daemon] takeover: ownership claimed, version recorded, entering normal daemon loop");
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
            if hashes.is_empty() { None } else { Some((name.to_string(), serde_json::json!(hashes))) }
        })
        .collect()
}

fn write_daemon_heartbeat(project_count: usize, plugin_module_count: usize) {
    let last_plugin_poll_ts = HEARTBEAT_LAST_PLUGIN_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let last_runner_poll_ts = HEARTBEAT_LAST_RUNNER_POLL_TS.load(std::sync::atomic::Ordering::Relaxed);
    let loaded_content_hashes: HashMap<String, String> =
        loaded_plugin_content_hashes().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let shared_pool_slot_hashes: HashMap<String, Vec<Option<String>>> = ["gm", "bert", "libsql", "treesitter"]
        .iter()
        .map(|name| (name.to_string(), agentplug_host::shared_plugin_slot_content_hashes(name)))
        .collect();
    let mixed_version_pools: Vec<String> = shared_pool_slot_hashes
        .iter()
        .filter(|(_, hashes)| hashes.iter().flatten().collect::<std::collections::HashSet<_>>().len() > 1)
        .map(|(name, _)| name.clone())
        .collect();
    let boot_ts = HEARTBEAT_DAEMON_BOOT_TS.load(std::sync::atomic::Ordering::Relaxed);
    let plugin_poll_error = last_plugin_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let runner_poll_error = last_runner_poll_error().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let staged_runner = staged_runner_awaiting_handoff();
    let handoff_attempt = last_handoff_attempt().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let _ = fs::write(
        daemon_status_path(),
        serde_json::json!({
            "pid": std::process::id(),
            "ts": now_ms(),
            "daemon_boot_ts": if boot_ts == 0 { serde_json::Value::Null } else { serde_json::json!(boot_ts) },
            "active_projects": project_count,
            "compiled_plugin_modules": plugin_module_count,
            "last_plugin_update_poll_ts": if last_plugin_poll_ts == 0 { serde_json::Value::Null } else { serde_json::json!(last_plugin_poll_ts) },
            "last_runner_update_poll_ts": if last_runner_poll_ts == 0 { serde_json::Value::Null } else { serde_json::json!(last_runner_poll_ts) },
            "last_plugin_update_poll_error": plugin_poll_error,
            "last_runner_update_poll_error": runner_poll_error,
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
        })
        .to_string(),
    );
}

fn last_completed_runner_swap_path() -> PathBuf {
    install_dir().join("last-completed-runner-swap.json")
}

/// Written the instant a staged runner build is promoted onto the canonical
/// exe path -- distinct from `staged_runner_awaiting_handoff` (which only
/// signals a swap IS pending), this is the durable "a swap just happened"
/// record an agent can diff against its own last-seen value to learn the
/// runner updated, without needing to poll daemon-status.json continuously.
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
    while path.extension().map(|e| e.eq_ignore_ascii_case("new")).unwrap_or(false) {
        path = path.with_extension("");
    }
    Some(path)
}

fn staged_matches_running(canonical: &Path, staged: &Path) -> bool {
    let Ok(running_meta) = fs::metadata(canonical) else { return false };
    let Ok(staged_meta) = fs::metadata(staged) else { return false };
    if running_meta.len() != staged_meta.len() {
        return false;
    }
    let Ok(running_bytes) = fs::read(canonical) else { return false };
    let Ok(staged_bytes) = fs::read(staged) else { return false };
    running_bytes == staged_bytes
}

fn staged_runner_awaiting_handoff() -> Option<(u64, u64)> {
    let canonical = canonical_runner_exe_path()?;
    let staged = canonical.with_extension(
        canonical.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string()),
    );
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

fn write_project_heartbeat(spool_dir: &Path, busy_until: Option<u64>) {
    write_project_heartbeat_with_queue_info(spool_dir, busy_until, None);
}

fn write_project_heartbeat_with_queue_info(spool_dir: &Path, busy_until: Option<u64>, queue_info: Option<(usize, usize)>) {
    let status_path = spool_dir.join(".status.json");
    let mut payload = serde_json::json!({
        "pid": std::process::id(),
        "ts": now_ms(),
        "daemon": true,
        "shared_process": true,
        "runtime": "agentplug",
    });
    if let Some(busy_until) = busy_until {
        payload["busy_until"] = serde_json::json!(busy_until);
    }
    if let Some((position, total)) = queue_info {
        payload["queue_position"] = serde_json::json!(position);
        payload["queue_depth"] = serde_json::json!(total);
    }
    payload["queue_wait_ms"] = serde_json::json!(last_measured_dispatch_queue_wait_ms());
    if let Some((staged_at_ms, _len)) = staged_runner_awaiting_handoff() {
        payload["runner_update_in_progress"] = serde_json::json!(true);
        payload["runner_update_waiting_ms"] = serde_json::json!(now_ms().saturating_sub(staged_at_ms));
    }
    if let Some(swap) = read_last_completed_runner_swap() {
        payload["last_completed_runner_swap"] = swap;
    }
    let pending_store_swaps = pending_store_swaps_by_plugin();
    if !pending_store_swaps.is_empty() {
        payload["pending_store_swaps"] = serde_json::Value::Object(pending_store_swaps);
    }
    let loaded_plugin_versions_informational_only_not_a_recovery_signal: serde_json::Map<String, serde_json::Value> =
        ["gm", "bert", "libsql", "treesitter"]
            .iter()
            .filter_map(|name| installed_plugin_version(name).map(|v| (name.to_string(), serde_json::json!(v))))
            .collect();
    if !loaded_plugin_versions_informational_only_not_a_recovery_signal.is_empty() {
        payload["loaded_plugin_versions"] = serde_json::Value::Object(loaded_plugin_versions_informational_only_not_a_recovery_signal);
    }
    let _ = fs::write(&status_path, payload.to_string());
}

fn known_project_roots() -> &'static Mutex<Vec<PathBuf>> {
    static SLOT: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_known_project_roots(roots: &[PathBuf]) {
    *known_project_roots().lock().unwrap_or_else(|e| e.into_inner()) = roots.to_vec();
}

/// Snapshot of every project root this daemon currently knows about, for
/// `download.rs`'s project-declared-plugin-spec lookup -- a project's own
/// `.agentplug/plugins.json` can only be found by scanning roots the daemon
/// has already discovered via its registry poll.
pub fn read_known_project_roots() -> Vec<PathBuf> {
    known_project_roots().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn spawn_project_heartbeat_ticker(interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        if heartbeat_authority_lost() {
            return;
        }
        let roots = known_project_roots().lock().unwrap_or_else(|e| e.into_inner()).clone();
        for root in roots {
            let spool_dir = root.join(".gm").join("exec-spool");
            if !spool_dir.exists() {
                continue;
            }
            write_project_heartbeat(&spool_dir, None);
        }
    })
}

static HEARTBEAT_PROJECT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_PLUGIN_MODULE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HEARTBEAT_LAST_PLUGIN_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_LAST_RUNNER_POLL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEARTBEAT_DAEMON_BOOT_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn last_plugin_poll_error() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn last_runner_poll_error() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_plugin_poll_error(err: Option<String>) {
    *last_plugin_poll_error().lock().unwrap_or_else(|e| e.into_inner()) = err;
}

fn record_runner_poll_error(err: Option<String>) {
    *last_runner_poll_error().lock().unwrap_or_else(|e| e.into_inner()) = err;
}

type HandoffAttempt = (u64, Option<String>);

fn last_handoff_attempt() -> &'static Mutex<Option<HandoffAttempt>> {
    static SLOT: OnceLock<Mutex<Option<HandoffAttempt>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_handoff_attempt(error: Option<String>) {
    *last_handoff_attempt().lock().unwrap_or_else(|e| e.into_inner()) = Some((now_ms(), error));
}

fn persisted_plugin_poll_ts_path() -> PathBuf {
    install_dir().join("last-plugin-update-poll-ts")
}

fn persisted_runner_poll_ts_path() -> PathBuf {
    install_dir().join("last-runner-update-poll-ts")
}

fn read_persisted_poll_ts(path: &Path) -> u64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)
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
    let elapsed_ms = if persisted_ts == 0 { NEVER_POLLED_BACKDATE_MS } else { now_ms().saturating_sub(persisted_ts) };
    instant_backdated_by_ms_capped_to_process_epoch(elapsed_ms)
}
static LOADED_PLUGIN_CONTENT_HASHES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn loaded_plugin_content_hashes() -> &'static Mutex<HashMap<String, String>> {
    LOADED_PLUGIN_CONTENT_HASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

static LAST_PLUGIN_COMPILE_FAILURE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn last_plugin_compile_failure() -> &'static Mutex<HashMap<String, String>> {
    LAST_PLUGIN_COMPILE_FAILURE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_plugin_compile_failure(plugin_name: &str, reason: String) {
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).insert(plugin_name.to_string(), reason);
    plugin_compile_backoff_until().lock().unwrap_or_else(|e| e.into_inner()).insert(plugin_name.to_string(), Instant::now() + PLUGIN_COMPILE_RETRY_BACKOFF);
}

fn clear_plugin_compile_failure(plugin_name: &str) {
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).remove(plugin_name);
    plugin_compile_backoff_until().lock().unwrap_or_else(|e| e.into_inner()).remove(plugin_name);
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
    last_plugin_compile_failure().lock().unwrap_or_else(|e| e.into_inner()).get(plugin_name).cloned()
}

static HEARTBEAT_AUTHORITY_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    // (mtime, len) of the wasm file the LAST time its content hash was
    // actually computed, keyed by plugin name -- lets get_or_compile skip
    // the full fs::read+sha256 (a 136MB read for bert.wasm, 56MB for
    // treesitter.wasm on this machine) on every call when the file plainly
    // has not changed since the last check. Root-caused 2026-08-14:
    // get_or_compile ran unconditionally once per sweep tick with no
    // cadence gate, so this was ~200MB of disk read + hashing on the main
    // sweep thread, serialized before any project's worker pool dispatch
    // even started -- directly on the path that made a freshly-dropped
    // spool file wait multiple seconds for its own dispatch to begin, on a
    // daemon whose actual per-call dispatch work (12-160ms, per
    // dispatch.end) was never the bottleneck.
    last_hash_check_stat: HashMap<String, (std::time::SystemTime, u64)>,
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
        })
    }

    fn get_or_compile(&mut self, plugin_name: &str) -> anyhow::Result<()> {
        let wasm_path = ensure_plugin_installed(plugin_name, None)?;
        let current_stat = wasm_file_stat(&wasm_path);
        let stat_unchanged = current_stat.is_some()
            && current_stat == self.last_hash_check_stat.get(plugin_name).copied();
        // Already compiled AND the file's (mtime, len) matches what it was
        // the last time the content hash was actually computed: skip the
        // read+hash entirely, this call cost one fs::metadata stat. A
        // genuinely swapped file with the SAME mtime+len as before (a
        // pathological same-second same-size rewrite) is not distinguished
        // from an unchanged file here -- acceptable because
        // ensure_plugin_installed's own download path always advances
        // mtime and almost always changes len for a real content swap, and
        // this is a staleness OPTIMIZATION layered on top of the already-
        // existing hash check, not a replacement for it: the very next call
        // whose stat differs still does the full re-hash.
        if stat_unchanged && self.modules.contains_key(plugin_name) {
            return Ok(());
        }
        let on_disk_hash = wasm_file_content_hash(&wasm_path)?;
        if let Some(stat) = current_stat {
            self.last_hash_check_stat.insert(plugin_name.to_string(), stat);
        }
        let old_loaded_hash = self.loaded_content_hash.get(plugin_name).cloned();
        let stale = old_loaded_hash.as_deref().is_some_and(|loaded_hash| loaded_hash != on_disk_hash);
        if stale {
            let old_hash = old_loaded_hash.unwrap_or_default();
            let (evicted_now, deferred) = agentplug_host::request_shared_store_swap(plugin_name, &old_hash);
            eprintln!(
                "[agentplug daemon] {plugin_name}.wasm content hash changed on disk since it was last compiled -- evicting the stale in-process module and draining the shared Stores using it ({evicted_now} slot(s) evicted now, {deferred} still in-flight and finishing on the old Store; their slots evict on completion), forcing a recompile from the current bytes"
            );
            self.modules.remove(plugin_name);
        }
        if !self.modules.contains_key(plugin_name) {
            if let Some(installed) = installed_plugin_version(plugin_name) {
                if !is_recognized_release_semver(&installed) {
                    eprintln!(
                        "[agentplug daemon] BOOT WARNING: {plugin_name}.wasm at {} is served from a NON-RELEASE version marker ({installed:?}) -- this is a local-dev sideload, not a released build, and the auto-updater will never overwrite it. If this was not intentional, replace the sideload with a real release-tagged {plugin_name}.wasm.",
                        wasm_path.display()
                    );
                }
            }
            eprintln!("[agentplug daemon] compiling {plugin_name}.wasm (shared across every project that uses it)...");
            let module = Module::from_file(&self.engine, &wasm_path)?;
            self.modules.insert(plugin_name.to_string(), module);
            self.loaded_content_hash.insert(plugin_name.to_string(), on_disk_hash.clone());
            loaded_plugin_content_hashes()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(plugin_name.to_string(), on_disk_hash.clone());
            // Rollback safety: if these exact bytes were previously deferred
            // behind an in-flight dispatch and are now current again, Stores
            // carrying them must stop being evicted on completion.
            agentplug_host::note_shared_plugin_bytes_current(plugin_name, &on_disk_hash);
        }
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
            let out_path = root.join(".gm").join("exec-spool").join("out").join(format!("{}-{}.json", req.verb, req.task));
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
    let requested_plugin = parsed.as_ref().and_then(|v| v.get("plugin").and_then(|p| p.as_str()).map(str::to_string));
    let also_runner = parsed.as_ref().and_then(|v| v.get("runner").and_then(|r| r.as_bool())).unwrap_or(false);

    let marker = force_plugin_refresh_marker_path();
    let contents = requested_plugin.as_deref().unwrap_or("").to_string();
    let _ = fs::write(&marker, contents);

    if also_runner {
        let _ = fs::write(force_runner_refresh_marker_path(), b"");
    }

    let local_dev_sideload = requested_plugin.as_deref().and_then(crate::download::read_local_dev_sideload_marker);

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
    Some(if contents.trim().is_empty() { None } else { Some(contents.trim().to_string()) })
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

fn write_spool_out(out_dir: &Path, out_name: &str, out_body: &str) {
    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
    if fs::write(&tmp, out_body).is_ok() {
        let _ = fs::rename(&tmp, out_dir.join(out_name));
        let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
    }
}

const ORPHAN_CLAIM_EXT: &str = "inflight";

fn inflight_claim_path(in_dir: &Path, verb: &str, task: &str) -> PathBuf {
    in_dir.join(verb).join(format!("{task}.txt.{ORPHAN_CLAIM_EXT}"))
}

pub fn sweep_orphaned_claims(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else { return };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
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
            let out_name = format!("{verb}-{task}.json");
            if !out_dir.join(&out_name).exists() {
                let out_body = serde_json::json!({
                    "ok": false,
                    "error_code": "dispatch_orphaned",
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that died before answering -- a wasm trap, an out-of-memory abort, a shared-Store recycle during the call, or a self-update handoff that exited while this dispatch was still running (check ~/.agentplug/daemon.log for a 'handed off to version' line at the matching time). The request was NOT completed and no partial work should be assumed. Re-dispatch it."),
                    "verb": verb,
                    "task": task,
                    "sweeping_pid": std::process::id(),
                }).to_string();
                write_spool_out(&out_dir, &out_name, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
            }
            let _ = fs::remove_file(&path);
        }
    }
}

pub fn sweep_unconsumable_spool_files(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let quarantine_dir = spool_dir.join("in-quarantine");
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else { return };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !file_entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if ext == Some("txt") || ext == Some(ORPHAN_CLAIM_EXT) {
                continue;
            }
            let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if file_name.is_empty() {
                continue;
            }
            let _ = fs::create_dir_all(&quarantine_dir);
            let dest = quarantine_dir.join(format!("{verb}__{file_name}"));
            if fs::rename(&path, &dest).is_ok() {
                eprintln!(
                    "[agentplug daemon] quarantined unconsumable spool file in/{verb}/{file_name} to {} -- the spool ABI is in/<verb>/<numeric-id>.txt, so a non-conforming name is never claimed by the dispatch loop and would otherwise sit invisibly forever",
                    dest.display()
                );
            }
        }
    }
}

/// Spool verb-directories reserved to bypass the `gm` plugin and dispatch
/// straight to a raw daemon-loaded plugin instead. A caller drops
/// `in/libsql/<N>.txt` with the actual libsql verb (`exec`/`query`/...)
/// carried INSIDE the JSON body as `"verb"` -- the directory name IS the
/// plugin name here, unlike every other spool verb-directory where the
/// directory name is a `gm` orchestrator verb and the plugin is implicitly
/// `gm`. This exists so a plain host process (no wasm runtime of its own,
/// e.g. freddie's Node.js) can reach a raw plugin like `libsql` through the
/// same file-drop spool protocol gm-skill itself already uses, instead of
/// spawning `agentplug-runner dispatch <plugin> <verb>` as a fresh subprocess
/// per call.
///
/// LATENCY, ROOT-CAUSED AND FIXED (2026-08-14): a wall-clock ~0.8-2.5s per
/// call was measured on this path (and identically on `gm`'s own verbs via
/// the same spool). The plugin's own internal `dispatch.end` log timing was
/// 12-160ms for the SAME calls, so the gap was not dispatch/wasm cost --
/// tracing further found the true dominant cost was `PluginModules::
/// get_or_compile` running unconditionally once per outer sweep tick,
/// serialized on the main sweep thread BEFORE any project's worker pool even
/// started, doing a full `fs::read` + SHA-256 hash of every default plugin's
/// `.wasm` file on every single call regardless of whether the file had
/// changed (bert.wasm is 136MB, treesitter.wasm 56MB on this machine -- that
/// is ~200MB of disk read + hashing on the critical path per tick). Fixed by
/// caching each plugin's `(mtime, len)` at the time its hash was last
/// actually computed and skipping the read+hash entirely when a fresh
/// `fs::metadata` stat matches -- reduces the common case to one cheap stat
/// call. A secondary, smaller contributor was `dispatch_project` paying a
/// `create_dir_all`x2 + heartbeat write + `plugins.txt` read for every
/// registered project on every tick even when that project's spool `in/`
/// directory was empty; also fixed with a read-first fast path that returns
/// immediately for the idle case. Measured post-fix on this machine: steady-
/// state calls to this path (after the first, which still pays the one-time
/// cold-compile cost per plugin) dropped from 900ms-2.5s to 128-183ms.
/// Neither the shared-daemon design (every project always shares one
/// process) nor the ~130-180ms remaining per-call floor changed -- this
/// spool path is still not a substitute for an in-process client library on
/// freddie's session-write hot path, just no longer pathologically slower
/// than it needed to be for occasional/non-hot-path callers.
const RAW_PLUGIN_SPOOL_VERBS: &[&str] = &["libsql", "bert"];

fn extract_session_id(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("session_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
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

static LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn last_measured_dispatch_queue_wait_ms() -> u64 {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn run_gm_dispatch_to_file(root: &Path, handle: &DispatchHandle, verb: &str, task: &str, body: &str, out_dir: &Path, queue_wait_ms: u64) {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.store(queue_wait_ms, std::sync::atomic::Ordering::Relaxed);
    let _fairness_guard = GmFairnessGuard::acquire(root);
    let plugin_name = if RAW_PLUGIN_SPOOL_VERBS.contains(&verb) { verb } else { "gm" };
    let inner_verb_owned: String = if plugin_name == "gm" {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("verb").and_then(|s| s.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "capabilities".to_string())
    };
    let dispatch_result = if plugin_name == "gm" {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch("gm", verb, body)))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.dispatch(plugin_name, &inner_verb_owned, body)))
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
    let out_name = format!("{verb}-{task}.json");
    write_spool_out(out_dir, &out_name, &out_body);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
    // Remove our own in-flight entry on completion. The dispatch loop's join
    // path (below, in dispatch_project) only removes entries whose join handle
    // it still owns -- a worker auto-detached after WORKER_AUTO_DETACH_AFTER_MS
    // has its handle dropped with the entry left behind, which used to leak it
    // PERMANENTLY: detached_still_running stayed true forever, so every later
    // runner self-update went down the 10-minute starve path and force-handed
    // off "despite N in-flight dispatch(es)" where N counted long-dead
    // entries, killing whatever real dispatches were running at that moment
    // (their callers saw dispatch_orphaned).
    let key: InFlightKey = (root.to_path_buf(), verb.to_string(), task.to_string());
    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
}

// Cheap probe for whether a cold project (no warm ProjectPlugins entry this
// tick) has a genuine client-side request already waiting in
// .agentplug/plugin-dispatch/in/<plugin>/<verb>/*.txt -- the same directory
// try_dispatch_via_daemon() polls with its own 30s MAX_WAIT_MS. Two
// fs::read_dir levels deep (plugin, then verb), stopping at the first
// claimable *.txt -- never walks file contents or the full tree, so this
// stays cheap even called on every cold root every tick. Matching only
// *.txt is load-bearing, not tidiness: the claim protocol renames
// <task>.txt to <task>.txt.inflight in place inside the same verb dir, so
// an any-entry probe reports every root that has ever dispatched as
// permanently pending. Measured live against a 103-root registry: 14 roots
// false-positive under any-entry, 0 under *.txt -- each force-scheduled
// into the bounded worker pool every tick ahead of real work.
fn dir_has_any_verb_subdir_with_claimable_txt(base: &Path) -> bool {
    let Ok(verb_dirs) = fs::read_dir(base) else { return false };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(verb_entry.path()) else { continue };
        for file_entry in files.flatten() {
            if file_entry.path().extension().and_then(|e| e.to_str()) == Some("txt") {
                return true;
            }
        }
    }
    false
}

// A cold project's real work can arrive on either of two independent
// dispatch surfaces: .agentplug/plugin-dispatch/in/<plugin>/<verb>/*.txt
// (try_dispatch_via_daemon's client-side path, checked below) or
// .gm/exec-spool/in/<verb>/*.txt (gm/plugkit's own spool ABI, the surface
// dispatch_project itself reads from). Checking only the first left a gm
// session's freshly-dropped .gm/exec-spool/in/ file invisible to this
// "genuinely active" probe -- a cold project could sit past every 30s
// cold-sweep window indefinitely if worker capacity was contended at each
// tick, since nothing marked it as having real waiting work.
fn project_has_pending_dispatch_work(root: &Path) -> bool {
    let pd_in = root.join(".agentplug").join("plugin-dispatch").join("in");
    if let Ok(plugin_dirs) = fs::read_dir(&pd_in) {
        for plugin_entry in plugin_dirs.flatten() {
            if !plugin_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if dir_has_any_verb_subdir_with_claimable_txt(&plugin_entry.path()) {
                return true;
            }
        }
    }
    let gm_in = root.join(".gm").join("exec-spool").join("in");
    dir_has_any_verb_subdir_with_claimable_txt(&gm_in)
}

fn dispatch_project(root: &Path, project: &mut ProjectPlugins, plugin_modules: &PluginModules) -> bool {
    let mut did_work = false;

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");

    // Fast path for the overwhelmingly common case (an idle project with
    // nothing claimed this tick): try the scan-and-claim pass FIRST, against
    // whatever in_dir already exists on disk, before paying for
    // create_dir_all x2 + a heartbeat write + a plugins.txt read. On a
    // machine serving many registered projects (87 observed live), this
    // per-project setup cost was paid unconditionally by every worker for
    // every project on every sweep tick regardless of whether that project
    // had any pending work -- with a small fixed worker pool, that idle-cost
    // multiplied by "many idle projects" is what queued a freshly-dropped
    // spool file behind every other project's turn before its own dispatch
    // even started (root-caused 2026-08-14: wall-clock 0.8-2.5s per call vs.
    // a 12-160ms internal dispatch.end timing for the SAME calls). Cutting
    // the idle case down to one read_dir attempt (no directory creation, no
    // heartbeat write, no plugins.txt read) shortens every idle worker's
    // turn, which shortens how long a busy project waits in the shared work
    // queue for a free worker.
    struct ClaimedRequest {
        verb: String,
        task: String,
        body: String,
        claimed_at: Instant,
    }
    let mut claimed: Vec<ClaimedRequest> = Vec::new();
    let in_dir_scan = fs::read_dir(&in_dir);
    let in_dir_existed = in_dir_scan.is_ok();
    if let Ok(entries) = in_dir_scan {
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
                let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                did_work = true;
                let claimed_at = Instant::now();
                let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();
                claimed.push(ClaimedRequest { verb: verb.clone(), task, body, claimed_at });
            }
        }
    }

    // Nothing claimed and the directory already existed (the common steady-
    // state idle case): skip create_dir_all/heartbeat/plugins.txt entirely,
    // this project cost one read_dir call this tick.
    //
    // MUST also check project_has_pending_dispatch_work before returning: the
    // .agentplug/plugin-dispatch/in/<plugin>/<verb>/ scan loop below (the one
    // try_dispatch_via_daemon's CLI `dispatch` clients actually poll against,
    // e.g. `agentplug-runner dispatch bert embed`) sits textually AFTER this
    // early return. Without this check, a project whose .gm/exec-spool/in is
    // idle (the common case for a bare CLI dispatch with no gm skill session
    // running) returns here EVERY tick and the plugin-dispatch scan is never
    // reached at all -- the client's own file sits unclaimed for the full 30s
    // MAX_WAIT_MS, times out, deletes its request, and falls through to a
    // full cold wasm reload. Root-caused 2026-08-24 via exact 34s-latency
    // reproduction matching MAX_WAIT_MS + cold-reload overhead precisely.
    if claimed.is_empty() && in_dir_existed && !project_has_pending_dispatch_work(root) {
        return did_work;
    }

    // Either in_dir didn't exist yet (first tick for this project, or it was
    // removed) or there is real work to do -- pay the setup cost now, exactly
    // as before this fast path was added.
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }
    write_project_heartbeat(&spool_dir, None);

    // Additive, never replacing. A non-empty .agentplug/plugins.txt used to
    // REPLACE this set, so a project naming the three side plugins silently
    // dropped `gm` itself and every dispatch failed to load -- observed live,
    // and the failure names the plugin that WAS listed rather than the one
    // that went missing, which makes it hard to attribute. Listing a plugin
    // should only ever add reach, never remove it.
    let requested_plugins = {
        let mut list = vec![
            "gm".to_string(),
            "libsql".to_string(),
            "bert".to_string(),
            "treesitter".to_string(),
            "oxibrowser".to_string(),
            "crux".to_string(),
        ];
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
        if let Some(out_body) = session_id_task_mismatch_rejection(&req.verb, &req.task, &req.body) {
            let out_name = format!("{}-{}.json", req.verb, req.task);
            write_spool_out(&out_dir, &out_name, &out_body);
            let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
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
            let out_name = format!("{}-{}.json", req.verb, req.task);
            write_spool_out(&out_dir, &out_name, &out_body);
            let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
        }
    };
    for req in plugin_refresh_requests {
        let out_body = handle_plugin_refresh_request(root, &req.body);
        let out_name = format!("{}-{}.json", req.verb, req.task);
        write_spool_out(&out_dir, &out_name, &out_body);
        let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
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
                if plugin_name == "gm" { gm_load_failure_reason = Some(reason); }
                continue;
            };
            if let Err(e) = project.load_plugin(&plugin_modules.engine, plugin_name, module, content_hash) {
                let reason = format!("failed to instantiate plugin {plugin_name} for {}: {e:#}", root.display());
                eprintln!("[agentplug daemon] {reason}");
                // A load/instantiate failure here (as opposed to a compile failure, already
                // handled above) means the plugin's *bytes* are fine but its host-import contract
                // doesn't match what this runner itself implements -- almost always the plugin's
                // own release channel having published a newer build than this runner's compiled-
                // in host ABI supports (see `record_plugin_load_failure_and_rollback`'s doc
                // comment for the full mechanism and how it was diagnosed). Roll back to the last
                // version that DID load, if one exists, so the project recovers on its own rather
                // than staying permanently broken until a human notices and manually restores
                // `plugin_name.wasm.prev` -- `get_or_compile`'s own content-hash staleness check
                // (this same loop's earlier `plugin_modules.module_with_hash` call, on ITS next
                // invocation) picks up the rolled-back bytes and recompiles automatically, no
                // separate cache-eviction call needed here.
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
                if plugin_name == "gm" { gm_load_failure_reason = Some(reason); }
            }
        }

        if !project.is_loaded("gm") {
            let error_message = match &gm_load_failure_reason {
                Some(reason) => format!("gm plugin failed to load for this project: {reason}"),
                None => "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)".to_string(),
            };
            for req in &gm_requests {
                let out_name = format!("{}-{}.json", req.verb, req.task);
                let out_body = serde_json::json!({"ok": false, "error": error_message, "verb": req.verb}).to_string();
                write_spool_out(&out_dir, &out_name, &out_body);
                let _ = fs::remove_file(inflight_claim_path(&in_dir, &req.verb, &req.task));
            }
            answer_bg_converts(bg_convert_requests);
        } else {
            struct Spawned {
                key: InFlightKey,
                join_handle: Option<std::thread::JoinHandle<()>>,
                detach_flag: Arc<std::sync::atomic::AtomicBool>,
                spawned_at: Instant,
            }
            let mut spawned: Vec<Spawned> = Vec::with_capacity(gm_requests.len());
            for req in gm_requests {
                let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((plugin_modules.engine.clone(), plugin_modules.modules_with_hashes())));
                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
                in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                let thread_root = root.to_path_buf();
                let thread_verb = req.verb.clone();
                let thread_task = req.task.clone();
                let thread_body = req.body.clone();
                let thread_out_dir = out_dir.clone();
                let queue_wait_ms = req.claimed_at.elapsed().as_millis() as u64;
                let join_handle = std::thread::spawn(move || {
                    run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir, queue_wait_ms);
                });
                spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
            }

            answer_bg_converts(bg_convert_requests);

            const WORKER_AUTO_DETACH_AFTER_MS: u64 = 45_000;
            const STATUS_REFRESH_INTERVAL_MS: u64 = 5_000;
            // Bounds how long this call keeps ABSORBING newly-arriving requests
            // for this one project before it stops looking for more and just
            // drains what it already has. Without this bound, a project under
            // sustained concurrent load (many sessions dispatching in a steady
            // stream) can keep the `while` loop below permanently non-empty --
            // every existing entry auto-detaches at its own +45s mark, but a
            // fresh one is added before that ever makes the loop's exit
            // condition true. Since the caller (`run_daemon_body`) runs a fixed
            // `worker_count` of these calls via `thread::scope` + `join()` over
            // ALL registered projects, one project that never returns from this
            // function permanently occupies one worker slot and -- once enough
            // busy projects do this to exceed `worker_count` -- starves every
            // OTHER registered project from ever being serviced by this shared
            // daemon, indefinitely. Witnessed live: 26 concurrent subagent
            // sessions hammering one project's spool wedged the whole daemon,
            // including unrelated projects with zero pending work of their own.
            // Once this deadline passes we stop claiming new `.txt` files (the
            // two rescans below) but keep waiting for already-spawned work,
            // which is itself bounded by WORKER_AUTO_DETACH_AFTER_MS -- so this
            // call now returns within a bounded window regardless of how much
            // new work keeps arriving, and the next outer tick picks the
            // project back up to continue draining it.
            const PROJECT_BATCH_ABSORB_WINDOW_MS: u64 = 3_000;
            let batch_deadline = Instant::now() + Duration::from_millis(PROJECT_BATCH_ABSORB_WINDOW_MS);
            let mut last_status_refresh = Instant::now();
            let bg_convert_dir = in_dir.join("background-convert");
            while spawned.iter().any(|s| s.join_handle.is_some()) {
                if last_status_refresh.elapsed() >= Duration::from_millis(STATUS_REFRESH_INTERVAL_MS) {
                    last_status_refresh = Instant::now();
                    write_project_heartbeat(&spool_dir, Some(now_ms() + STATUS_REFRESH_INTERVAL_MS));
                }
                for s in spawned.iter_mut() {
                    if s.join_handle.is_some()
                        && !s.detach_flag.load(std::sync::atomic::Ordering::SeqCst)
                        && s.spawned_at.elapsed() >= Duration::from_millis(WORKER_AUTO_DETACH_AFTER_MS)
                    {
                        eprintln!(
                            "[agentplug daemon] gm dispatch for {} exceeded {WORKER_AUTO_DETACH_AFTER_MS}ms with no completion -- auto-detaching so this worker and the daemon's other projects are not blocked; it keeps running and will write its out/ file whenever it finishes",
                            root.display()
                        );
                        s.detach_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        s.join_handle = None;
                    }
                }
                for s in spawned.iter_mut() {
                    let Some(jh) = s.join_handle.as_ref() else { continue };
                    if jh.is_finished() {
                        let jh = s.join_handle.take().unwrap();
                        let _ = jh.join();
                        in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(&s.key);
                    } else if s.detach_flag.load(std::sync::atomic::Ordering::SeqCst) {
                        s.join_handle = None;
                    }
                }
                if spawned.iter().any(|s| s.join_handle.is_some()) {
                    if Instant::now() < batch_deadline {
                        if let Ok(files) = fs::read_dir(&bg_convert_dir) {
                            for file_entry in files.flatten() {
                                let file_path = file_entry.path();
                                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                                    continue;
                                }
                                let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                                if fs::rename(&file_path, &claim_path).is_err() {
                                    continue;
                                }
                                let bc_task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                let bc_body = fs::read_to_string(&claim_path).unwrap_or_default();
                                let out_body = handle_background_convert(root, &bc_body);
                                let out_name = format!("background-convert-{bc_task}.json");
                                write_spool_out(&out_dir, &out_name, &out_body);
                                let _ = fs::remove_file(&claim_path);
                            }
                        }

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
                                    let claim_path = file_path.with_extension(format!("txt.{ORPHAN_CLAIM_EXT}"));
                                    if fs::rename(&file_path, &claim_path).is_err() {
                                        continue;
                                    }
                                    let claimed_at = Instant::now();
                                    let task = file_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                    let body = fs::read_to_string(&claim_path).unwrap_or_default();

                                    if let Some(out_body) = session_id_task_mismatch_rejection(&verb, &task, &body) {
                                        let out_name = format!("{verb}-{task}.json");
                                        write_spool_out(&out_dir, &out_name, &out_body);
                                        let _ = fs::remove_file(&claim_path);
                                        continue;
                                    }

                                    let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((plugin_modules.engine.clone(), plugin_modules.modules_with_hashes())));
                                    let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    let key: InFlightKey = (root.to_path_buf(), verb.clone(), task.clone());
                                    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), InFlightHandle { detach: detach_flag.clone() });

                                    let thread_root = root.to_path_buf();
                                    let thread_verb = verb.clone();
                                    let thread_task = task.clone();
                                    let thread_body = body;
                                    let thread_out_dir = out_dir.clone();
                                    let queue_wait_ms = claimed_at.elapsed().as_millis() as u64;
                                    let join_handle = std::thread::spawn(move || {
                                        run_gm_dispatch_to_file(&thread_root, &self_healing_dispatch_handle, &thread_verb, &thread_task, &thread_body, &thread_out_dir, queue_wait_ms);
                                    });
                                    spawned.push(Spawned { key, join_handle: Some(join_handle), detach_flag, spawned_at: Instant::now() });
                                }
                            }
                        }
                    }

                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

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

                {
                    let current = plugin_modules.module_with_hash(&plugin_name)
                        .map(|(_, hash)| project.is_loaded_current(&plugin_name, hash))
                        .unwrap_or_else(|| project.is_loaded(&plugin_name));
                    if !current {
                        let Some((module, content_hash)) = plugin_modules.module_with_hash(&plugin_name) else {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            continue;
                        };
                        if let Err(e) = project.load_plugin(&plugin_modules.engine, &plugin_name, module, content_hash) {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            continue;
                        }
                    }
                }

                if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&DaemonConfig::load()) {
                    let mut released: Vec<&str> = Vec::new();
                    for shared_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
                        if shared_name != plugin_name && agentplug_host::release_shared_plugin(shared_name) {
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

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| project.dispatch(&plugin_name, &verb, &body)));
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
                write_pd_out(&out_name, &out_body);
            }
        }
    }

    did_work
}

pub fn try_dispatch_via_daemon(cwd: &Path, plugin: &str, verb: &str, body: &str) -> Option<String> {
    if std::env::var("AGENTPLUG_NO_DAEMON").is_ok() {
        return None;
    }
    if let Err(e) = register_project(cwd) {
        eprintln!("[agentplug] {e}");
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
    let _ = fs::remove_file(&req_path);
    None
}

fn seed_github_token_from_gh_cli_if_unset() {
    if std::env::var_os("GITHUB_TOKEN").is_some() || std::env::var_os("GH_TOKEN").is_some() {
        return;
    }
    let Ok(output) = std::process::Command::new("gh").args(["auth", "token"]).output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return;
    }
    std::env::set_var("GH_TOKEN", &token);
    eprintln!("[agentplug daemon] seeded GH_TOKEN from `gh auth token` -- ci-status and other GitHub API verbs now run authenticated, avoiding the unauthenticated 60/hr rate limit");
}

pub fn run_daemon() -> anyhow::Result<()> {
    eprintln!("[agentplug daemon] starting, registry {}", registry_path().display());
    seed_github_token_from_gh_cli_if_unset();

    if !claim_ownership() {
        let existing_pid = read_owner_pid();
        eprintln!(
            "[agentplug daemon] lost the atomic ownership claim -- pid {:?} already owns the shared daemon, exiting before touching any shared plugin state",
            existing_pid
        );
        return Ok(());
    }

    let plugin_modules = PluginModules::new()?;
    let previously_recorded_version = installed_runner_version();
    if previously_recorded_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        crate::download::clear_all_known_bad_version_markers();
        let _ = record_runner_version(env!("CARGO_PKG_VERSION"));
    }
    run_daemon_body(plugin_modules)
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

    let daemon_cfg = DaemonConfig::load();
    let registry_poll_interval = daemon_cfg.registry_poll_interval();
    let heartbeat_interval = daemon_cfg.heartbeat_interval();
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

    // A rename failure in download_and_verify (self-update staging, plugin
    // hot-swap) deliberately leaves its sha256-verified tmp.<pid> file on
    // disk rather than destroying it -- nothing today retries the rename
    // against that same file, so left unswept it leaks forever. 1 hour is
    // comfortably longer than any real download_and_verify call could still
    // be in flight, so this never touches a concurrent writer's own tmp file.
    crate::download::gc_stale_tmp_files(Duration::from_secs(60 * 60));

    const COLD_PROJECT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

    let mut projects: HashMap<PathBuf, ProjectPlugins> = HashMap::new();
    let mut last_registry_poll = Instant::now();
    let mut first_registry_poll_pending = true;
    let mut known_roots: Vec<PathBuf> = Vec::new();
    let mut roots_new_this_registry_poll: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut last_cold_project_sweep = Instant::now().checked_sub(COLD_PROJECT_SWEEP_INTERVAL).unwrap_or_else(Instant::now);

    const SELF_RECYCLE_IDLE_MS: u64 = 60 * 60 * 1000;
    let mut last_any_dispatch = Instant::now();

    let shared_plugin_release_idle_ms = daemon_cfg.shared_plugin_release_idle_ms();
    let mut last_shared_release = Instant::now();

    let plugin_update_poll_interval = daemon_cfg.plugin_update_poll_interval();
    let instruction_source_poll_interval = daemon_cfg.instruction_source_poll_interval();
    // The outer gate uses the SHORTEST configured interval across the
    // default and every per-plugin override, so no plugin's own poll is ever
    // delayed past its configured cadence by a longer default -- the actual
    // per-plugin decision (skip a not-yet-due plugin this tick) happens
    // inside the loop below via last_plugin_specific_poll.
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
    let persisted_runner_poll_ts_at_boot = read_persisted_poll_ts(&persisted_runner_poll_ts_path());
    if persisted_runner_poll_ts_at_boot > 0 {
        HEARTBEAT_LAST_RUNNER_POLL_TS.store(persisted_runner_poll_ts_at_boot, std::sync::atomic::Ordering::Relaxed);
    }
    let mut pending_self_update: Option<(PathBuf, String)> = None;
    let mut pending_self_update_staged_at: Option<Instant> = None;
    const SELF_UPDATE_MAX_STARVED_MS: u64 = 10 * 60 * 1000;

    if let Some((staged_at_ms, _len)) = staged_runner_awaiting_handoff() {
        if let Some(staged_path) = canonical_runner_exe_path().map(|c| {
            c.with_extension(c.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string()))
        }) {
            let staged_age = now_ms().saturating_sub(staged_at_ms);
            match std::process::Command::new(&staged_path).arg("--version").output() {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout).trim().trim_start_matches('v').to_string();
                    eprintln!(
                        "[agentplug daemon] found pre-existing staged runner {} (version {version}) at boot, age {}ms -- adopting its on-disk mtime so a daemon restart does not reset the starve clock",
                        staged_path.display(), staged_age
                    );
                    pending_self_update = Some((staged_path, version));
                    pending_self_update_staged_at = Instant::now().checked_sub(Duration::from_millis(staged_age));
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

    let mut last_browser_orphan_sweep = Instant::now()
        .checked_sub(Duration::from_millis(5 * 60 * 1000))
        .unwrap_or_else(Instant::now);

    let _heartbeat_ticker = spawn_heartbeat_ticker(heartbeat_interval);
    write_daemon_heartbeat(0, 0);

    const PROJECT_HEARTBEAT_TICK_INTERVAL_MS: u64 = 3_000;
    let _project_heartbeat_ticker = spawn_project_heartbeat_ticker(Duration::from_millis(PROJECT_HEARTBEAT_TICK_INTERVAL_MS));

    loop {
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            for root in &known_roots {
                sweep_orphaned_claims(root);
            }
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting before serving further work");
            return Ok(());
        }

        if first_registry_poll_pending || last_registry_poll.elapsed() >= registry_poll_interval {
            let sweep_orphans_left_by_whatever_daemon_died_before_answering = first_registry_poll_pending;
            first_registry_poll_pending = false;
            last_registry_poll = Instant::now();
            let previous_roots: std::collections::HashSet<PathBuf> = known_roots.iter().cloned().collect();
            known_roots = read_registry();
            roots_new_this_registry_poll = known_roots.iter().filter(|r| !previous_roots.contains(*r)).cloned().collect();
            set_known_project_roots(&known_roots);
            if sweep_orphans_left_by_whatever_daemon_died_before_answering {
                for root in &known_roots {
                    sweep_orphaned_claims(root);
                    sweep_unconsumable_spool_files(root);
                }
            }
        }

        const BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS: u64 = 5 * 60 * 1000;
        if last_browser_orphan_sweep.elapsed() >= Duration::from_millis(BROWSER_ORPHAN_SWEEP_INTERVAL_LONGER_THAN_REGISTRY_POLL_MS) {
            last_browser_orphan_sweep = Instant::now();
            agentplug_host::reap_idle_sessions_and_os_orphans_across_every_known_project_root(&known_roots);
        }

        let max_concurrent_projects = daemon_cfg.max_concurrent_projects();

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
            if due {
                last_instruction_source_sync.insert(root.clone(), Instant::now());
                let thread_root = root.clone();
                std::thread::spawn(move || {
                    if let Err(e) = sync_instruction_source_if_configured(&thread_root) {
                        eprintln!("[agentplug daemon] instruction source-repo sync failed for {}: {e:#}", thread_root.display());
                    }
                });
            }
        }
        for plugin_name in ["gm", "libsql", "bert", "treesitter"] {
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

        // A project with an in-memory ProjectPlugins entry has dispatched
        // something within the last PLUGIN_IDLE_EVICT_MS (that's what keeps
        // it from being evicted, below) -- warm, checked every tick. A
        // project with none, and that this daemon has already seen in a
        // prior registry poll, has been silent for that whole window --
        // cold. On a machine that accumulates every project directory it has
        // ever served (register_project only drops an entry once its path
        // stops existing on disk, never on inactivity) the cold set
        // dominates the registry almost immediately. Scanning all of it
        // every tick pays a read_dir per cold project per tick for work that
        // essentially never arrives there, which is exactly the
        // queue-depth-looks-deep-but-isn't-real symptom this sweep interval
        // exists to cut: cold projects are swept far less often, so a small
        // worker pool spends its ticks on roots actually worth checking. A
        // root that is genuinely new this registry poll (roots_new_this_
        // registry_poll) is force-included regardless of the cold-sweep
        // cadence -- a fresh project's very first dispatch must not wait for
        // the next 30s sweep just because it has no ProjectPlugins entry yet.
        let sweep_cold_this_tick = last_cold_project_sweep.elapsed() >= COLD_PROJECT_SWEEP_INTERVAL;
        if sweep_cold_this_tick {
            last_cold_project_sweep = Instant::now();
        }
        let mut all_projects: Vec<(PathBuf, ProjectPlugins)> = Vec::with_capacity(known_roots.len());
        // Tracks, in parallel with all_projects, whether each entry has a genuine
        // reason to be scheduled THIS tick (an existing warm ProjectPlugins, a
        // root new this registry poll, or real pending dispatch work) versus
        // being pulled in only because sweep_cold_this_tick fired for the whole
        // tick. all_projects/worker_count/queue itself stay exactly as before
        // (scheduling must still cold-refresh everything on the sweep cadence) --
        // this only separates what gets REPORTED as queue_depth/queue_position
        // from what gets scheduled, so a routine cold-sweep tick does not report
        // the full lifetime project registry as live backlog.
        let mut is_genuinely_active: Vec<bool> = Vec::with_capacity(known_roots.len());
        let mut skipped_cold = 0usize;
        for root in &known_roots {
            match projects.remove(root) {
                Some(p) => {
                    all_projects.push((root.clone(), p));
                    is_genuinely_active.push(true);
                }
                // A cold project with a real client-side dispatch request
                // already waiting in .agentplug/plugin-dispatch/in/ must not
                // sit unprocessed until the next 30s cold-sweep -- that races
                // directly against try_dispatch_via_daemon's own 30s
                // MAX_WAIT_MS client timeout, and a cold project under
                // multi-project contention (several sibling daemons'
                // projects consuming max_concurrent_projects worker slots
                // every tick) can lose that race on nearly every call,
                // falling through to a full cold wasm-module reload every
                // single dispatch even though the daemon was alive and idle
                // the whole time. project_has_pending_dispatch_work is a
                // cheap fs::read_dir emptiness probe, not a full directory
                // walk, so checking it on every cold root every tick stays
                // negligible relative to the 50ms+ tick cadence elsewhere in
                // this loop.
                None if sweep_cold_this_tick
                    || roots_new_this_registry_poll.contains(root)
                    || project_has_pending_dispatch_work(root) =>
                {
                    all_projects.push((root.clone(), ProjectPlugins::new(root.clone())));
                    // Genuinely active only if it's new-this-poll or has real
                    // pending work -- a root pulled in purely by the cold-sweep
                    // cadence (sweep_cold_this_tick, no pending work of its own)
                    // is a staleness-refresh, not backlog.
                    is_genuinely_active.push(
                        roots_new_this_registry_poll.contains(root) || project_has_pending_dispatch_work(root),
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
                    write_project_heartbeat_with_queue_info(&spool_dir, None, Some((position, reported_queue_total)));
                }
            }
        }
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
            if reported_queue_total > worker_count {
                let spool_dir = root.join(".gm").join("exec-spool");
                if fs::create_dir_all(&spool_dir).is_ok() {
                    write_project_heartbeat_with_queue_info(&spool_dir, None, Some((0, 0)));
                }
            }
            projects.insert(root, project);
        }
        HEARTBEAT_PROJECT_COUNT.store(projects.len(), std::sync::atomic::Ordering::Relaxed);
        HEARTBEAT_PLUGIN_MODULE_COUNT.store(plugin_modules.modules.len(), std::sync::atomic::Ordering::Relaxed);
        if heartbeat_authority_lost() {
            agentplug_host::close_all_sessions();
            eprintln!("[agentplug daemon] heartbeat authority held by another daemon -- exiting after finishing in-flight batch");
            return Ok(());
        }
        let evict_before = Instant::now().checked_sub(Duration::from_millis(daemon_cfg.project_idle_evict_ms())).unwrap_or_else(Instant::now);
        let to_evict: Vec<PathBuf> = projects.iter().filter(|(_, p)| p.last_active < evict_before).map(|(root, _)| root.clone()).collect();
        for root in to_evict {
            eprintln!("[agentplug daemon] evicting idle project {}", root.display());
            projects.remove(&root);
        }

        let forced_refresh_request = take_forced_plugin_refresh_request();
        if last_plugin_update_poll.elapsed() >= shortest_plugin_poll_interval || forced_refresh_request.is_some() {
            last_plugin_update_poll = Instant::now();
            let poll_ts = now_ms();
            HEARTBEAT_LAST_PLUGIN_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
            write_persisted_poll_ts(&persisted_plugin_poll_ts_path(), poll_ts);
            let targets: Vec<String> = match &forced_refresh_request {
                Some(Some(name)) => vec![name.clone()],
                _ => plugin_modules.modules.keys().cloned().collect(),
            };
            let mut cycle_errors: Vec<String> = Vec::new();
            for plugin_name in targets {
                // Forced refresh always bypasses this plugin's own cadence
                // (the agent explicitly asked for it now); the ordinary tick
                // only polls a plugin whose OWN interval has actually
                // elapsed -- a project setting bert to 3600s no longer gets
                // its poll-check re-fired every time libsql's shorter
                // interval trips the outer gate.
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
                            "[agentplug daemon] downloaded+verified plugin {plugin_name} update to {new_version} -- the next tick's get_or_compile content-hash check evicts and recompiles it unconditionally, no idle window required"
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

        if last_runner_update_poll.elapsed() >= runner_update_poll_interval || take_forced_runner_refresh_request() {
            last_runner_update_poll = Instant::now();
            let poll_ts = now_ms();
            HEARTBEAT_LAST_RUNNER_POLL_TS.store(poll_ts, std::sync::atomic::Ordering::Relaxed);
            write_persisted_poll_ts(&persisted_runner_poll_ts_path(), poll_ts);
            match crate::download::stage_runner_self_update() {
                Ok(Some((staged, version))) => {
                    eprintln!("[agentplug daemon] staged self-update to {version} at {}", staged.display());
                    if pending_self_update.is_none() {
                        pending_self_update_staged_at = Some(Instant::now());
                    }
                    pending_self_update = Some((staged, version));
                    record_runner_poll_error(None);
                }
                Ok(None) => record_runner_poll_error(None),
                Err(e) => {
                    let msg = format!("runner self-update check failed: {e}");
                    eprintln!("[agentplug daemon] {msg}");
                    record_runner_poll_error(Some(msg));
                }
            }
        }

        let self_update_starved = pending_self_update_staged_at
            .map(|staged_at| staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_MAX_STARVED_MS))
            .unwrap_or(false);
        // `any_work` only covers dispatches this loop iteration still owns a
        // join handle for. An auto-detached dispatch (WORKER_AUTO_DETACH_AFTER_MS)
        // keeps running on a thread the loop has let go of, so `any_work` goes
        // false while a real call is still executing -- handing off there kills
        // it and the caller gets a `dispatch_orphaned` whose text blames a wasm
        // trap/OOM/Store-recycle rather than the handoff that actually did it.
        // in_flight_map keeps the detached entry until its thread is joined, so
        // it is the signal that survives detachment.
        let detached_still_running = !in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).is_empty();
        // A runner self-update is never urgent -- the current binary keeps serving
        // correctly, this is purely picking up a newer build. Once starved, a
        // second, longer grace period gives whatever's still genuinely in-flight
        // (heavy verbs like `instruction` running a codeinsight rebuild routinely
        // take 15-30s+) a chance to finish naturally on later loop iterations
        // instead of being killed the instant the first deadline passes -- that
        // instant-kill was the recurring `instruction`-specific dispatch_orphaned
        // pattern this comment used to just document as an unavoidable cost. The
        // hard cap still forces the handoff eventually so a genuinely wedged
        // dispatch cannot block updates forever.
        const SELF_UPDATE_HARD_CAP_MS: u64 = SELF_UPDATE_MAX_STARVED_MS + 60_000;
        let self_update_hard_capped = pending_self_update_staged_at
            .map(|staged_at| staged_at.elapsed() >= Duration::from_millis(SELF_UPDATE_HARD_CAP_MS))
            .unwrap_or(false);
        let force_handoff_despite_in_flight = self_update_starved && detached_still_running && self_update_hard_capped;
        let force_handoff_never_idle = self_update_starved && self_update_hard_capped && !detached_still_running && any_work;
        if (!any_work && !detached_still_running) || force_handoff_despite_in_flight || force_handoff_never_idle {
            if let Some((staged, version)) = pending_self_update.take() {
                if force_handoff_despite_in_flight {
                    eprintln!(
                        "[agentplug daemon] self-update to {version} starved for {}ms with in-flight dispatches still running after the extra grace window -- forcing handoff despite {} in-flight dispatch(es); their callers will see dispatch_orphaned",
                        SELF_UPDATE_HARD_CAP_MS,
                        in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).len()
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
                    eprintln!("[agentplug daemon] handed off to version {version} -- exiting");
                    return Ok(());
                }
                pending_self_update = Some((staged, version));
            }
        }

        if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(&daemon_cfg) {
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
        } else if last_shared_release.elapsed() >= Duration::from_millis(shared_plugin_release_idle_ms) {
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
        } else if last_any_dispatch.elapsed() >= Duration::from_millis(SELF_RECYCLE_IDLE_MS) && !detached_still_running {
            // detached_still_running is the in_flight_map signal that survives
            // worker auto-detachment -- the same gap the self-update handoff
            // gate above already guards. Without it, a >45s dispatch that
            // outlives its join handle is invisible to `any_work`, and this
            // idle self-recycle exits the process out from under it (its
            // caller gets dispatch_orphaned blamed on a wasm trap/OOM).
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
