use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use agentplug_host::now_ms;

use crate::daemon::{in_flight_map, inflight_claim_path, write_spool_out_confirmed, InFlightKey};

const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const OVERRIDES_REFRESH: Duration = Duration::from_secs(30);
const EXPLICIT_TIMEOUT_GRACE_MS: u64 = 120_000;
const QUEUED_BEHIND_LANE_CEILING_MS: u64 = 3_600_000;
const FALLBACK_CEILING_SECS: u64 = 600;
const PROCESS_AND_BROWSER_CEILING_SECS: u64 = 1200;
const INDEXING_CEILING_SECS: u64 = 3600;
const STORE_AND_GIT_CEILING_SECS: u64 = 900;

const PROCESS_AND_BROWSER_VERBS: &[&str] = &[
    "exec_js", "lang", "nodejs", "javascript", "node", "js", "typescript", "python", "py", "bash", "sh", "shell", "zsh",
    "powershell", "ps1", "ssh", "go", "rust", "c", "cpp", "java", "deno", "serp", "browser", "cdp", "fetch", "wait",
];
const INDEXING_VERBS: &[&str] = &["codesearch", "code_index", "codeinsight_index", "index", "embed", "scan_deps"];
const STORE_VERBS: &[&str] = &["memorize", "memorize-fire", "memorize-prune", "memorize-vacuum", "recall", "forget", "health"];

static REAPED: OnceLock<Mutex<HashSet<InFlightKey>>> = OnceLock::new();

fn reaped_set() -> &'static Mutex<HashSet<InFlightKey>> {
    REAPED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn take_reaped(key: &InFlightKey) -> bool {
    reaped_set().lock().unwrap_or_else(|e| e.into_inner()).remove(key)
}

pub(crate) struct DispatchClock {
    pub(crate) claimed_at_ms: u64,
    pub(crate) started_at_ms: Arc<AtomicU64>,
    pub(crate) explicit_timeout_ms: Option<u64>,
}

impl DispatchClock {
    pub(crate) fn at_claim(body: &str) -> Self {
        Self { claimed_at_ms: now_ms(), started_at_ms: Arc::new(AtomicU64::new(0)), explicit_timeout_ms: explicit_timeout_ms(body) }
    }
}

pub(crate) fn explicit_timeout_ms(body: &str) -> Option<u64> {
    for line in body.lines().take(3) {
        let line = line.trim();
        if let Some(ms) = line.strip_prefix("timeoutMs=").and_then(|v| v.trim().parse::<u64>().ok()) {
            return Some(ms);
        }
        if let Some(secs) = line.strip_prefix("timeout=").and_then(|v| v.trim().parse::<u64>().ok()) {
            return Some(secs.saturating_mul(1000));
        }
    }
    let json = serde_json::from_str::<serde_json::Value>(body).ok()?;
    if let Some(ms) = json.get("timeoutMs").or_else(|| json.get("timeout_ms")).and_then(|v| v.as_u64()) {
        return Some(ms);
    }
    json.get("timeout_seconds").or_else(|| json.get("timeout")).and_then(|v| v.as_u64()).map(|s| s.saturating_mul(1000))
}

struct CeilingOverrides {
    by_verb: HashMap<String, u64>,
    default_secs: Option<u64>,
}

fn read_overrides() -> CeilingOverrides {
    let path = agentplug_host::install_dir().join("daemon-config.json");
    let parsed = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}')).ok());
    let by_verb = parsed
        .as_ref()
        .and_then(|v| v.get("dispatch_ceiling_secs_by_verb"))
        .and_then(|v| v.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_u64().map(|s| (k.clone(), s))).collect())
        .unwrap_or_default();
    let default_secs = parsed.as_ref().and_then(|v| v.get("dispatch_ceiling_secs")).and_then(|v| v.as_u64());
    CeilingOverrides { by_verb, default_secs }
}

fn default_ceiling_secs(verb: &str) -> u64 {
    if PROCESS_AND_BROWSER_VERBS.contains(&verb) {
        PROCESS_AND_BROWSER_CEILING_SECS
    } else if INDEXING_VERBS.contains(&verb) {
        INDEXING_CEILING_SECS
    } else if STORE_VERBS.contains(&verb) || verb.starts_with("git_") {
        STORE_AND_GIT_CEILING_SECS
    } else {
        FALLBACK_CEILING_SECS
    }
}

fn ceiling_ms(verb: &str, explicit_timeout_ms: Option<u64>, overrides: &CeilingOverrides) -> u64 {
    if let Some(explicit) = explicit_timeout_ms {
        return explicit.saturating_add(EXPLICIT_TIMEOUT_GRACE_MS);
    }
    let secs = overrides
        .by_verb
        .get(verb)
        .copied()
        .or(overrides.default_secs)
        .unwrap_or_else(|| default_ceiling_secs(verb));
    secs.saturating_mul(1000)
}

fn reap(key: &InFlightKey, claim_age_ms: u64, run_age_ms: Option<u64>, ceiling_ms: u64) {
    let (root, verb, task) = key;
    in_flight_map().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    reaped_set().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone());
    let reason = match run_age_ms {
        Some(run) => format!("verb {verb} ran {run} ms without finishing, past its {ceiling_ms} ms ceiling (dispatch_ceiling_secs_by_verb in daemon-config.json overrides it, an explicit timeoutMs or timeout on the dispatch raises it)"),
        None => format!("verb {verb} was claimed {claim_age_ms} ms ago and never started executing, past its {ceiling_ms} ms queued ceiling"),
    };
    let out_body = serde_json::json!({
        "ok": false,
        "error_code": "dispatch_reaped",
        "reaped": true,
        "reason": reason,
        "claim_age_ms": claim_age_ms,
        "run_age_ms": run_age_ms,
        "ceiling_ms": ceiling_ms,
        "error": format!("{reason}. The outcome is UNVERIFIED: a side-effecting verb may have applied some or all of its work, so read the real state before re-dispatching; a read-only verb can be re-dispatched straight away."),
        "verb": verb,
        "task": task,
    })
    .to_string();
    let spool = root.join(".gm").join("exec-spool");
    let out_name = format!("{verb}-{task}.json");
    eprintln!("[agentplug watchdog] reaping {verb}/{task} for {}: {reason}", root.display());
    if write_spool_out_confirmed(&spool.join("out"), &out_name, &out_body) {
        let _ = std::fs::remove_file(inflight_claim_path(&spool.join("in"), verb, task));
    }
}

pub(crate) fn spawn() {
    let spawned = std::thread::Builder::new().name("dispatch-watchdog".to_string()).spawn(|| {
        let mut overrides = read_overrides();
        let mut overrides_read_at = Instant::now();
        loop {
            std::thread::sleep(SCAN_INTERVAL);
            if overrides_read_at.elapsed() >= OVERRIDES_REFRESH {
                overrides = read_overrides();
                overrides_read_at = Instant::now();
            }
            let now = now_ms();
            let snapshot: Vec<(InFlightKey, u64, u64, Option<u64>)> = in_flight_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(key, handle)| {
                    (key.clone(), handle.clock.claimed_at_ms, handle.clock.started_at_ms.load(Ordering::Relaxed), handle.clock.explicit_timeout_ms)
                })
                .collect();
            for (key, claimed_at, started_at, explicit) in snapshot {
                let ceiling = ceiling_ms(&key.1, explicit, &overrides);
                if started_at > 0 {
                    let run_age = now.saturating_sub(started_at);
                    if run_age > ceiling {
                        reap(&key, now.saturating_sub(claimed_at), Some(run_age), ceiling);
                    }
                } else {
                    let claim_age = now.saturating_sub(claimed_at);
                    let queued_ceiling = ceiling.max(QUEUED_BEHIND_LANE_CEILING_MS);
                    if claim_age > queued_ceiling {
                        reap(&key, claim_age, None, queued_ceiling);
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("[agentplug daemon] could not start the dispatch watchdog: {e} -- a live dispatch that never finishes will not be reaped");
    }
}
