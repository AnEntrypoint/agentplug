use super::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(crate) type InFlightKey = (PathBuf, String, String);

pub(crate) struct InFlightHandle {
    pub(crate) detach: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

pub(crate) fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) const MAX_CLAIMED_DISPATCHES_PER_PROJECT: usize = 32;

/// The lane-wait budget for every lane except codesearch and git, and the floor every other
/// budget is read against.
///
/// A caller's `timeout_seconds` is a poll budget, not a cancellation: a dispatch that outlives it
/// keeps waiting and the caller resumes it with `resume_task`. It tops out at 240s, so the lane may
/// never answer inside one poll -- a caller that asked for the maximum budget used to be starved at
/// 120s with `waited_ms: 120005` and no way to raise it. The lane budget now spans that whole
/// documented budget, so a caller polling at the maximum and resuming once reaches the same wait it
/// asked for instead of losing the race on a timer it could not see. Nothing runs while a dispatch
/// waits, so a longer budget costs no correctness: `executed: false` and `re_dispatch_safe: true`
/// hold at any budget.
pub(super) const LANE_WAIT_MAX_MS_DEFAULT: u64 = 240_000;
pub(super) const GIT_LANE_WAIT_MAX_MS_DEFAULT: u64 = 600_000;
pub(super) const CODESEARCH_LANE_WAIT_MS_DEFAULT: u64 = 5_000;
pub(super) const DISPATCH_WAIT_LEDGER_FILE: &str = ".dispatch-wait.json";
pub(super) const LEDGER_REFRESH_MIN_INTERVAL_MS: u64 = 250;

/// A lane hold past this long is a stalled holder, not a slow operation, and the watchdog reports
/// it. The bound is a report threshold only: a live holder is never released, because one writer
/// per lane is what keeps concurrent git writers on one worktree serialized.
pub(super) const LANE_HOLD_REPORT_MIN_MS: u64 = 120_000;
/// The watchdog names a long hold at most this often, so one stalled `git_finalize` produces a
/// readable line per minute instead of one per ledger refresh.
pub(super) const LANE_HOLD_REPORT_INTERVAL_MS: u64 = 60_000;

/// A hold past this long is no longer a slow operation: the holder is either gone, has never
/// reached `Running`, or reached `Running` and then stopped being a live worker, and the lane is
/// released with a named reason. A hold by a dispatch that IS running and still registered is never
/// released here -- the lane is what keeps two git writers out of one worktree, and a live holder is
/// exactly the writer it protects; that hold has its own absolute ceiling in
/// `LANE_HOLD_RUNNING_MAX_MS_DEFAULT`. The env override exists only so the selfcheck can witness the
/// release without a three-minute wait. No shipped path sets it.
pub(super) const LANE_HOLD_STUCK_MIN_MS_DEFAULT: u64 = 180_000;

/// A hold by a dispatch that IS running is released past this long, when at least one claim is
/// waiting on the lane.
///
/// The exemption above it is unbounded, so one dispatch that never finishes -- a `git push` blocked
/// on a remote, measured at 724236ms on one project -- converts a serialization point into a
/// project-wide stall: every state-changing git verb behind it answers starved, is told
/// `re_dispatch_safe: true`, and re-queues behind the same hold, so the retry advice does not
/// converge. Ten minutes is past any real git operation on any box this runs on, so a hold that
/// long is a hung holder and not slow work; the lane is worth more than the tail of a dispatch that
/// is already broken. Releasing it is chosen over leaving it because the alternative has no bound:
/// the stall lasts as long as the holder does, and the holder has shown it can last arbitrarily
/// long. The `waiters` gate keeps the release from ever firing when nobody is blocked, so the
/// single-writer property is only spent to unblock somebody.
pub(super) const LANE_HOLD_RUNNING_MAX_MS_DEFAULT: u64 = 600_000;

/// The reserved body member the caller's own poll budget arrives in. gm-mcp writes it so the lane
/// wait spans the budget the caller actually asked for -- a caller polling at the documented maximum
/// used to be starved at 120s by a budget it had no way to raise -- and the runner strips it before
/// the body is hashed for its FIFO identity and before it reaches any plugin, so the channel can
/// never change what a request is or what a plugin sees.
pub(super) const LANE_WAIT_MS_BODY_KEY: &str = "_agentplug_lane_wait_ms";
/// The public spelling of the same channel, so a caller writing its own spool body buys the wait
/// without owning the reserved name. The reserved member wins when both appear.
pub(super) const LANE_WAIT_MS_PUBLIC_BODY_KEY: &str = "lane_wait_ms";
/// A caller's budget is honoured down to this floor, so a sub-second poll cannot turn every lane
/// wait into an instant starvation.
pub(super) const LANE_WAIT_FLOOR_MS: u64 = 1_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DispatchStage {
    WaitingForLane,
    WaitingForToolQueue,
    Running,
}

pub(super) struct DispatchWaitRecord {
    claimed_at_ms: u64,
    stage: DispatchStage,
    stage_since_ms: u64,
    lane: Option<&'static str>,
    thread: std::thread::ThreadId,
}

pub(super) fn dispatch_wait_records() -> &'static Mutex<HashMap<InFlightKey, DispatchWaitRecord>> {
    static RECORDS: OnceLock<Mutex<HashMap<InFlightKey, DispatchWaitRecord>>> = OnceLock::new();
    RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn note_dispatch_stage(
    root: &Path,
    verb: &str,
    task: &str,
    stage: DispatchStage,
    lane: Option<&'static str>,
) {
    let key = (root.to_path_buf(), verb.to_string(), task.to_string());
    let now = now_ms();
    {
        let mut records = dispatch_wait_records()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let claimed_at_ms = records
            .get(&key)
            .map(|record| record.claimed_at_ms)
            .unwrap_or(now);
        records.insert(
            key,
            DispatchWaitRecord {
                claimed_at_ms,
                stage,
                stage_since_ms: now,
                lane,
                thread: std::thread::current().id(),
            },
        );
    }
    publish_dispatch_wait_ledger(root);
}

pub(super) fn ledger_last_refresh_ms() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static LAST: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn publish_dispatch_wait_ledger(root: &Path) {
    let now = now_ms();
    {
        let mut last = ledger_last_refresh_ms()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(last.get(root).copied().unwrap_or(0)) < LEDGER_REFRESH_MIN_INTERVAL_MS
        {
            return;
        }
        last.insert(root.to_path_buf(), now);
    }
    refresh_dispatch_wait_ledger(root);
}

pub(super) fn dispatch_wait_record(root: &Path, verb: &str, task: &str) -> Option<DispatchWaitRecord> {
    let key = (root.to_path_buf(), verb.to_string(), task.to_string());
    let records = dispatch_wait_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let record = records.get(&key)?;
    Some(DispatchWaitRecord {
        claimed_at_ms: record.claimed_at_ms,
        stage: record.stage,
        stage_since_ms: record.stage_since_ms,
        lane: record.lane,
        thread: record.thread,
    })
}

pub(super) fn forget_dispatch_wait_record(root: &Path, verb: &str, task: &str) {
    let key = (root.to_path_buf(), verb.to_string(), task.to_string());
    dispatch_wait_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

pub(super) fn dispatch_stage_name(stage: DispatchStage) -> &'static str {
    match stage {
        DispatchStage::WaitingForLane => "claimed_waiting_for_serial_lane",
        DispatchStage::WaitingForToolQueue => "claimed_waiting_for_tool_queue",
        DispatchStage::Running => "claimed_running",
    }
}

pub(super) fn project_in_flight_count(root: &Path) -> usize {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|(active_root, _, _)| active_root == root)
        .count()
}

pub(super) struct InFlightEntryRelease {
    key: InFlightKey,
}

impl Drop for InFlightEntryRelease {
    fn drop(&mut self) {
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

pub(super) fn handle_background_convert(root: &Path, body: &str) -> String {
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
            handle
                .detach
                .store(true, std::sync::atomic::Ordering::SeqCst);
            serde_json::json!({"ok": true, "converted": true, "verb": req.verb, "task": req.task})
                .to_string()
        }
        None => {
            let out_path = root
                .join(".gm")
                .join("exec-spool")
                .join("out")
                .join(format!("{}-{}.json", req.verb, req.task));
            if out_path.exists() {
                serde_json::json!({"ok": false, "error": "already_completed", "verb": req.verb, "task": req.task}).to_string()
            } else {
                serde_json::json!({"ok": false, "error": "unknown_task", "reason": "no in-flight dispatch and no out/ file found for this verb+task -- this task id was never dispatched, or its verb never matched", "verb": req.verb, "task": req.task}).to_string()
            }
        }
    }
}

pub(super) fn handle_plugin_refresh_request(root: &Path, body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let requested_plugin = parsed
        .as_ref()
        .and_then(|v| v.get("plugin").and_then(|p| p.as_str()).map(str::to_string));
    let also_runner = parsed
        .as_ref()
        .and_then(|v| v.get("runner").and_then(|r| r.as_bool()))
        .unwrap_or(false);

    let marker = force_plugin_refresh_marker_path();
    let contents = requested_plugin.as_deref().unwrap_or("").to_string();
    let _ = fs::write(&marker, contents);

    if also_runner {
        let _ = fs::write(force_runner_refresh_marker_path(), b"");
    }

    let local_dev_sideload = requested_plugin
        .as_deref()
        .and_then(crate::download::read_local_dev_sideload_marker);

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

pub(super) fn force_plugin_refresh_marker_path() -> PathBuf {
    install_dir().join("force-plugin-refresh.request")
}

pub(super) fn take_forced_plugin_refresh_request() -> Option<Option<String>> {
    let marker = force_plugin_refresh_marker_path();
    let contents = fs::read_to_string(&marker).ok()?;
    let _ = fs::remove_file(&marker);
    Some(if contents.trim().is_empty() {
        None
    } else {
        Some(contents.trim().to_string())
    })
}

pub(super) fn force_runner_refresh_marker_path() -> PathBuf {
    install_dir().join("force-runner-refresh.request")
}

pub(super) fn take_forced_runner_refresh_request() -> bool {
    let marker = force_runner_refresh_marker_path();
    if marker.exists() {
        let _ = fs::remove_file(&marker);
        true
    } else {
        false
    }
}

pub(super) const FOREIGN_SWEEPER_STALE_MS: u64 = 120_000;

pub fn live_foreign_spool_sweeper(spool_dir: &Path) -> Option<u64> {
    let status = fs::read_to_string(spool_dir.join(".status.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&status).ok()?;
    let pid = value.get("pid").and_then(|p| p.as_u64())?;
    // A standalone watcher never sweeps (main.rs reads only `.txt` requests), so its heartbeat
    // must not suppress this daemon's sweep.
    if !value
        .get("shared_process")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    if pid == std::process::id() as u64 {
        return None;
    }
    let ts = value.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    if now_ms().saturating_sub(ts) >= FOREIGN_SWEEPER_STALE_MS {
        return None;
    }
    if !pid_is_alive(pid) {
        return None;
    }
    Some(pid)
}

pub(super) fn spool_in_file_write_has_settled(request_path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(request_path) else {
        return false;
    };
    metadata.len() > 0 && spool_in_file_has_no_writer(request_path)
}

#[cfg(windows)]
pub(super) fn spool_in_file_has_no_writer(request_path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(request_path)
        .is_ok()
}

#[cfg(not(windows))]
pub(super) fn spool_in_file_has_no_writer(_request_path: &Path) -> bool {
    true
}

pub(super) fn language_spool_extension(verb: &str) -> Option<&'static str> {
    match verb {
        "nodejs" => Some("js"),
        "python" => Some("py"),
        "bash" => Some("sh"),
        "powershell" => Some("ps1"),
        "typescript" | "deno" => Some("ts"),
        "go" => Some("go"),
        "rust" => Some("rs"),
        "c" => Some("c"),
        "cpp" => Some("cpp"),
        "java" => Some("java"),
        _ => None,
    }
}

pub(super) const UNIVERSAL_SPOOL_REQUEST_EXTENSION: &str = "txt";

pub(super) fn spool_request_extension(verb: &str) -> &'static str {
    language_spool_extension(verb).unwrap_or(UNIVERSAL_SPOOL_REQUEST_EXTENSION)
}

pub(super) fn accepted_spool_request_extensions(verb: &str) -> [&'static str; 2] {
    [
        spool_request_extension(verb),
        UNIVERSAL_SPOOL_REQUEST_EXTENSION,
    ]
}

pub(super) fn is_spool_request_path(verb: &str, request_path: &Path) -> bool {
    let Some(extension) = request_path
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return false;
    };
    accepted_spool_request_extensions(verb).contains(&extension)
}

pub(super) fn spool_claim_path(request_path: &Path) -> Option<PathBuf> {
    let extension = request_path.extension()?.to_str()?;
    Some(request_path.with_extension(format!("{extension}.{ORPHAN_CLAIM_EXT}")))
}

pub fn claim_spool_request_in_place(request_path: &Path) -> Option<PathBuf> {
    if !spool_in_file_write_has_settled(request_path) {
        return None;
    }
    let claim_path = spool_claim_path(request_path)?;
    fs::rename(request_path, &claim_path)
        .ok()
        .map(|_| claim_path)
}

pub(super) const EXEC_OUTPUT_SPILL_THRESHOLD_CHARS: usize = 2000;

pub(super) fn exec_output_field_text(envelope: &serde_json::Value, field: &str) -> Option<String> {
    match envelope.get(field)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) if text.is_empty() => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

pub(super) fn spill_large_exec_output_to_text_sibling(
    out_dir: &Path,
    verb: &str,
    task: &str,
    out_body: String,
) -> String {
    let Ok(mut outer) = serde_json::from_str::<serde_json::Value>(&out_body) else {
        return out_body;
    };
    let Some(envelope_text) = outer.get("data").and_then(|d| d.as_str()) else {
        return out_body;
    };
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(envelope_text) else {
        return out_body;
    };
    if !envelope.get("stdout").is_some_and(|v| v.is_string()) {
        return out_body;
    }
    let sections: Vec<(&str, String)> = ["result", "stdout", "stderr"]
        .into_iter()
        .filter_map(|field| exec_output_field_text(&envelope, field).map(|text| (field, text)))
        .collect();
    if !sections
        .iter()
        .any(|(_, text)| text.chars().count() > EXEC_OUTPUT_SPILL_THRESHOLD_CHARS)
    {
        return out_body;
    }
    let mut rendered = String::new();
    for (field, text) in &sections {
        rendered.push_str(&format!("## {field}\n{text}\n\n"));
    }
    let sibling_name = format!("{verb}-{task}.txt");
    let sibling = out_dir.join(&sibling_name);
    if fs::write(&sibling, rendered).is_err() {
        return out_body;
    }
    let Some(obj) = outer.as_object_mut() else {
        return out_body;
    };
    obj.insert(
        "result_file".to_string(),
        serde_json::Value::String(sibling.to_string_lossy().into_owned()),
    );
    outer.to_string()
}

/// Entries kept in `.dispatch-ledger.json` -- the same cap the plugin-side ledger uses, so a
/// host-minted id ages out of the window on the same schedule as a plugin-minted one.
const DISPATCH_LEDGER_MAX_ENTRIES: usize = 500;
/// Above this the receipt is not re-parsed to stamp an id: a megabyte-plus body is already spilling
/// to a sibling file, and the id is a citation aid, never a reason to reserialize that much text.
const DISPATCH_ID_STAMP_MAX_BODY_BYTES: usize = 1_048_576;

/// Only the plugin mints a dispatch id (`dispatch_ledger::record`, reached from the guest reply), so
/// every receipt the host builds because the plugin never answered -- an epoch deadline interrupt, a
/// panic, an empty result, an admission or lane starvation answer, an orphaned claim -- carried no
/// dispatch_id and left a failed dispatch uncitable, which breaks the (id, hash, ts) audit primitive.
/// The stamping lives in the out-file writer rather than in each failing verb, so every written
/// receipt gets one id of the same shape a successful receipt carries: `<ts>-<seq>-<hash>`.
pub fn stamp_dispatch_id(out_dir: &Path, verb: &str, task: &str, out_body: &str) -> String {
    if out_body.len() > DISPATCH_ID_STAMP_MAX_BODY_BYTES {
        return out_body.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(out_body) else {
        return out_body.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return out_body.to_string();
    };
    if obj
        .get("dispatch_id")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return out_body.to_string();
    }
    let Some(dispatch_id) = mint_host_dispatch_id(out_dir, verb, task, out_body.len() as u64) else {
        return out_body.to_string();
    };
    obj.insert(
        "dispatch_id".to_string(),
        serde_json::Value::String(dispatch_id),
    );
    serde_json::to_string(&value).unwrap_or_else(|_| out_body.to_string())
}

/// Mints an id with the plugin ledger's own algorithm -- DefaultHasher (fixed keys, so the value is
/// reproducible) over verb, fingerprint and ts -- and appends it to that same ledger file, so an
/// ok:false dispatch resolves through `lookup()` like any other. The spool task stands in for the
/// request fingerprint: at the writer the request body is gone, and the task is what identifies the
/// request. The ledger write is best-effort for the same reason the out-file write is confirmed by a
/// rename: the plugin writes this file too, and a lost ledger row costs a resolvable id, never the
/// id itself, which the receipt carries either way.
fn mint_host_dispatch_id(
    out_dir: &Path,
    verb: &str,
    task: &str,
    output_bytes: u64,
) -> Option<String> {
    let root = out_dir.ancestors().nth(3)?;
    let path = root
        .join(".gm")
        .join("exec-spool")
        .join(".dispatch-ledger.json");
    let mut list: Vec<serde_json::Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = list.len() as u64;
    let dispatch_id = format!("{}-{}-{:x}", ts, seq, {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        verb.hash(&mut h);
        task.hash(&mut h);
        ts.hash(&mut h);
        h.finish()
    });
    list.push(serde_json::json!({
        "dispatch_id": dispatch_id,
        "verb": verb,
        "fingerprint": task,
        "ts": ts,
        "exit_code": 1,
        "output_bytes": output_bytes,
        "session_id": null,
        "host_minted": true,
    }));
    if list.len() > DISPATCH_LEDGER_MAX_ENTRIES {
        let drop = list.len() - DISPATCH_LEDGER_MAX_ENTRIES;
        list.drain(0..drop);
    }
    let serialized = serde_json::Value::Array(list).to_string();
    let tmp = path.with_file_name(format!(
        ".dispatch-ledger.json.tmp.{}",
        std::process::id()
    ));
    if fs::write(&tmp, serialized).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    Some(dispatch_id)
}

pub fn write_spool_out_confirmed(
    out_dir: &Path,
    verb: &str,
    task: &str,
    out_body: &str,
) -> bool {
    let out_name = format!("{verb}-{task}.json");
    let out_body = stamp_dispatch_id(out_dir, verb, task, out_body);
    let dest = out_dir.join(&out_name);
    let tmp = out_dir.join(format!("{out_name}.tmp.{}", std::process::id()));
    if fs::write(&tmp, out_body).is_ok() && fs::rename(&tmp, &dest).is_ok() {
        let _ = fs::write(out_dir.join(format!("{out_name}.ready")), b"");
        let confirmed = dest.exists();
        if confirmed {
            if let Some(root) = out_dir.ancestors().nth(3) {
                reap_spool_out_files(root, false);
            }
        }
        return confirmed;
    }
    let _ = fs::remove_file(&tmp);
    dest.exists()
}

pub(super) fn forget_in_flight_claim(in_dir: &Path, verb: &str, task: &str) {
    let Some(root) = in_dir.ancestors().nth(3) else {
        return;
    };
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(root.to_path_buf(), verb.to_string(), task.to_string()));
}

enum StoreLockState {
    Clear,
    Reclaimed,
    Held {
        lock_dir: PathBuf,
        owner_pid: Option<u64>,
        detail: String,
    },
}

/// How stale a store lock's owner heartbeat must be before the lock is treated as abandoned. It is
/// far longer than any real write because it is not a timeout on the write: the owner record is
/// re-stamped every STORE_OWNER_HEARTBEAT_MS while a dispatch holds the lock, so a live writer never
/// ages past this. It exists only to escape a pid that was recycled by an unrelated process, which
/// is the one case where "owner pid is alive" is true and the owner is still gone.
const STORE_LOCK_STALE_MS: u64 = 900_000;

const STORE_OWNER_HEARTBEAT_MS: u64 = 15_000;

fn read_store_lock_owner(owner_file: &Path) -> Option<(u64, u64)> {
    let text = fs::read_to_string(owner_file).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse::<u64>().ok()?;
    let ts = lines
        .next()
        .and_then(|line| line.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Some((pid, ts))
}

fn reclaim_store_lock(lock_dir: &Path, owner_file: &Path, reason: &str) -> StoreLockState {
    let removed = fs::remove_dir(lock_dir).is_ok() || fs::remove_dir_all(lock_dir).is_ok();
    if !removed {
        return StoreLockState::Held {
            lock_dir: lock_dir.to_path_buf(),
            owner_pid: None,
            detail: format!(
                "store lock {} is stale ({reason}) but could not be removed; remove it by hand",
                lock_dir.display()
            ),
        };
    }
    let _ = fs::remove_file(owner_file);
    eprintln!("[agentplug daemon] reclaimed store lock {}: {reason}", lock_dir.display());
    StoreLockState::Reclaimed
}

/// A `<db>.lock` directory is libsql's mutual-exclusion marker: the writer removes it when it
/// finishes, so a process killed mid-write leaves it forever and every later write fails with no
/// holder to wait on. The check is therefore not "is the directory there" but "is its owner still
/// here": every dispatch that writes gm.db claims `<db>.lock.owner` (see StoreOwnerRecord), so a lock
/// with no record was never claimed by a live writer, a record naming a dead pid is provably
/// abandoned, and a live pid whose heartbeat stopped is abandoned too. A lock whose recorded owner is
/// alive and heartbeating is never touched -- only its owner may remove it.
fn settle_store_lock(root: &Path) -> StoreLockState {
    let gm_dir = root.join(".gm");
    let lock_dir = gm_dir.join("gm.db.lock");
    if !lock_dir.is_dir() {
        return StoreLockState::Clear;
    }
    let owner_file = gm_dir.join("gm.db.lock.owner");
    let owner_pid = read_store_lock_owner(&owner_file).map(|(pid, _)| pid);
    let own_pid = u64::from(std::process::id());
    match read_store_lock_owner(&owner_file) {
        None => reclaim_store_lock(
            &lock_dir,
            &owner_file,
            &format!(
                "it has no owner record, and every dispatch that writes gm.db claims one, so no live writer holds it (a lock predating owner records)"
            ),
        ),
        Some((pid, _)) if pid == own_pid => StoreLockState::Held {
            lock_dir: lock_dir.clone(),
            owner_pid,
            detail: format!(
                "store lock {} is held by another dispatch of this daemon (pid {pid}); gm.db cannot be written until it releases it",
                lock_dir.display()
            ),
        },
        Some((pid, ts)) if pid_is_alive(pid) && !(ts > 0 && agentplug_host::now_ms().saturating_sub(ts) >= STORE_LOCK_STALE_MS) => {
            StoreLockState::Held {
                lock_dir: lock_dir.clone(),
                owner_pid,
                detail: format!(
                    "store lock {} is held by live pid {pid}; gm.db cannot be written until that process releases it",
                    lock_dir.display()
                ),
            }
        }
        Some((pid, ts)) => {
            let reason = if !pid_is_alive(pid) {
                format!("its owner pid {pid} is no longer alive")
            } else {
                format!(
                    "its owner pid {pid} is alive but stopped heartbeating {} ms ago, past the {} ms stale bound",
                    agentplug_host::now_ms().saturating_sub(ts),
                    STORE_LOCK_STALE_MS
                )
            };
            reclaim_store_lock(&lock_dir, &owner_file, &reason)
        }
    }
}

fn store_owner_refcounts() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static REFS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    REFS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ownership of .gm/gm.db.lock for the lifetime of one dispatch. Refcounted because the
/// unserialized codeinsight family runs several store writes at once: the first claim writes the
/// record and the last release removes it, so a concurrent sibling never sees a lock with no owner.
/// The heartbeat re-stamps the timestamp so a long index pass is never judged stale.
struct StoreOwnerRecord {
    path: PathBuf,
    pid: u64,
    stop: Arc<AtomicBool>,
}

impl StoreOwnerRecord {
    fn claim(root: &Path) -> Self {
        let path = root.join(".gm").join("gm.db.lock.owner");
        let pid = u64::from(std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let first = {
            let mut refs = store_owner_refcounts()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let entry = refs.entry(path.clone()).or_insert(0);
            *entry += 1;
            *entry == 1
        };
        let record = Self {
            path: path.clone(),
            pid,
            stop: stop.clone(),
        };
        record.stamp();
        if first {
            let heartbeat_path = path;
            let heartbeat_stop = stop;
            let heartbeat_pid = pid;
            std::thread::spawn(move || {
                while !heartbeat_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(
                        STORE_OWNER_HEARTBEAT_MS,
                    ));
                    if heartbeat_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = fs::write(
                        &heartbeat_path,
                        format!("{}\n{}\n", heartbeat_pid, agentplug_host::now_ms()),
                    );
                }
            });
        }
        record
    }

    fn stamp(&self) {
        let _ = fs::write(&self.path, format!("{}\n{}\n", self.pid, agentplug_host::now_ms()));
    }
}

impl Drop for StoreOwnerRecord {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let last = {
            let mut refs = store_owner_refcounts()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match refs.get_mut(&self.path) {
                Some(count) => {
                    *count = count.saturating_sub(1);
                    let last = *count == 0;
                    if last {
                        refs.remove(&self.path);
                    }
                    last
                }
                None => true,
            }
        };
        if last {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn write_spool_out_and_release_claim(
    out_dir: &Path,
    in_dir: &Path,
    verb: &str,
    task: &str,
    out_body: &str,
) {
    forget_in_flight_claim(in_dir, verb, task);
    if write_spool_out_confirmed(out_dir, verb, task, out_body) {
        let claim = inflight_claim_path(in_dir, verb, task);
        let spool_dir = in_dir
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| in_dir.to_path_buf());
        remove_claim_file(&spool_dir, verb, task, &claim);
    } else {
        eprintln!("[agentplug daemon] out-file write for {verb}/{task} did not confirm -- leaving the claim for the orphan sweep instead of deleting an unanswered request");
    }
}

pub(super) const ORPHAN_CLAIM_EXT: &str = "inflight";

pub(super) fn inflight_claim_path_with_extension(
    in_dir: &Path,
    verb: &str,
    task: &str,
    extension: &str,
) -> PathBuf {
    in_dir
        .join(verb)
        .join(format!("{task}.{extension}.{ORPHAN_CLAIM_EXT}"))
}

pub(super) fn existing_inflight_claim(
    in_dir: &Path,
    verb: &str,
    task: &str,
) -> Option<(PathBuf, &'static str)> {
    accepted_spool_request_extensions(verb)
        .into_iter()
        .map(|extension| {
            (
                inflight_claim_path_with_extension(in_dir, verb, task, extension),
                extension,
            )
        })
        .find(|(claim, _)| claim.exists())
}

pub(super) fn inflight_claim_path(in_dir: &Path, verb: &str, task: &str) -> PathBuf {
    existing_inflight_claim(in_dir, verb, task)
        .map(|(claim, _)| claim)
        .unwrap_or_else(|| {
            inflight_claim_path_with_extension(in_dir, verb, task, spool_request_extension(verb))
        })
}

/// A claim's liveness marker, and the cadence the dispatch that owns it renews on.
///
/// A claim is the request file renamed, and its mtime is what orders the lane queue, so liveness
/// cannot ride on the claim itself: re-stamping it would send a long wait to the back of its own
/// lane. The marker sits beside the claim instead, so a sweep in ANY process can tell a 120s
/// `wait` from the corpse of a daemon that died holding one -- both used to read as one claim
/// nobody had touched for a minute, which is why a bounded wait was reaped inside its own budget.
pub(super) const CLAIM_HEARTBEAT_DIR: &str = "claim-live";

pub(super) const CLAIM_HEARTBEAT_EXT: &str = "live";

/// How often the owner re-stamps its marker. Well inside MIN_ORPHAN_CLAIM_AGE_MS, so a claim whose
/// owner is running can never drift into the orphan window between two beats.
pub(super) const CLAIM_HEARTBEAT_MS: u64 = 15_000;

/// A marker older than this has missed three beats, so its owner is gone or wedged and the claim is
/// judged on its own age, exactly as it was before claims had markers at all.
pub(super) const CLAIM_HEARTBEAT_STALE_MS: u64 = 45_000;

pub(super) fn claim_heartbeat_path(spool_dir: &Path, verb: &str, task: &str) -> PathBuf {
    spool_dir
        .join(CLAIM_HEARTBEAT_DIR)
        .join(verb)
        .join(format!("{task}.{CLAIM_HEARTBEAT_EXT}"))
}

/// One beat: the owning pid and the stamp, the same two-line record the store-owner heartbeat
/// writes.
fn write_claim_heartbeat(marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(marker, format!("{}\n{}\n", std::process::id(), now_ms()));
}

/// The pid that last renewed this claim and how long ago, or None when it carries no marker. No
/// marker means no live dispatch ever renewed it, which is the orphan case this replaced.
pub(super) fn claim_heartbeat(spool_dir: &Path, verb: &str, task: &str) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(claim_heartbeat_path(spool_dir, verb, task)).ok()?;
    let mut lines = raw.lines();
    let pid = lines.next()?.trim().parse::<u64>().ok()?;
    let stamp = lines.next()?.trim().parse::<u64>().ok()?;
    Some((pid, now_ms().saturating_sub(stamp)))
}

/// Deletes a claim together with its marker: a marker left behind keeps naming a live owner for a
/// task id the spool reuses, and only its going stale would ever stop that.
fn remove_claim_file(spool_dir: &Path, verb: &str, task: &str, claim_path: &Path) {
    let _ = fs::remove_file(claim_heartbeat_path(spool_dir, verb, task));
    let _ = fs::remove_file(claim_path);
}

/// Renews a claim's marker for as long as one dispatch runs.
///
/// Held from the moment a dispatch thread starts -- which is while it waits for its lane, not only
/// while it executes -- until that thread returns, so the whole life of the dispatch is covered.
/// Dropping it stops the beats and takes the marker with it, so a finished dispatch leaves nothing
/// behind that could protect an unrelated claim later.
pub(crate) struct ClaimHeartbeat {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    marker: PathBuf,
}

impl ClaimHeartbeat {
    pub(crate) fn start(in_dir: &Path, verb: &str, task: &str) -> ClaimHeartbeat {
        let claim = inflight_claim_path(in_dir, verb, task);
        let spool_dir = in_dir
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| in_dir.to_path_buf());
        let marker = claim_heartbeat_path(&spool_dir, verb, task);
        let stop = Arc::new(AtomicBool::new(false));
        write_claim_heartbeat(&marker);
        let beat_stop = Arc::clone(&stop);
        let beat_marker = marker.clone();
        let beat_claim = claim;
        let thread = std::thread::Builder::new()
            .name(format!("claim-heartbeat-{task}"))
            .spawn(move || {
                let beat = std::time::Duration::from_millis(CLAIM_HEARTBEAT_MS);
                let poll = std::time::Duration::from_millis(250);
                let mut since = std::time::Duration::ZERO;
                while !beat_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(poll);
                    since += poll;
                    if since < beat {
                        continue;
                    }
                    since = std::time::Duration::ZERO;
                    // The claim IS the dispatch: once it is gone the dispatch has answered, so stop
                    // beating instead of renewing a claim nobody holds any more.
                    if !beat_claim.exists() {
                        break;
                    }
                    write_claim_heartbeat(&beat_marker);
                }
                let _ = fs::remove_file(&beat_marker);
            })
            .ok();
        ClaimHeartbeat {
            stop,
            thread,
            marker,
        }
    }
}

impl Drop for ClaimHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Joined, not detached: a detached beat could re-create the marker this drop removes,
            // which would leave a finished dispatch still claiming a live owner.
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.marker);
    }
}

pub(super) fn queued_request_path_with_extension(
    in_dir: &Path,
    verb: &str,
    task: &str,
    extension: &str,
) -> PathBuf {
    in_dir.join(verb).join(format!("{task}.{extension}"))
}

pub(super) fn any_queued_request_exists(in_dir: &Path, verb: &str, task: &str) -> bool {
    accepted_spool_request_extensions(verb)
        .into_iter()
        .any(|extension| queued_request_path_with_extension(in_dir, verb, task, extension).exists())
}

pub(super) fn project_in_dir(root: &Path) -> PathBuf {
    root.join(".gm").join("exec-spool").join("in")
}

pub(super) type AbandonedClaim = (PathBuf, String, String);

pub(super) fn requeue_claim(in_dir: &Path, verb: &str, task: &str) -> bool {
    let Some((claim, extension)) = existing_inflight_claim(in_dir, verb, task) else {
        return false;
    };
    let spool_dir = in_dir
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| in_dir.to_path_buf());
    // This claim is being handed to another daemon, so the beats stop here: the successor's own
    // marker is the only liveness evidence for the work from now on.
    let _ = fs::remove_file(claim_heartbeat_path(&spool_dir, verb, task));
    if any_queued_request_exists(in_dir, verb, task) {
        let _ = fs::remove_file(&claim);
        return true;
    }
    fs::rename(
        &claim,
        queued_request_path_with_extension(in_dir, verb, task, extension),
    )
    .is_ok()
}

pub(super) fn snapshot_in_flight_claims() -> Vec<AbandonedClaim> {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect()
}

pub(super) fn requeue_claims_for_live_successor(claims: &[AbandonedClaim]) -> usize {
    claims
        .iter()
        .filter(|(root, verb, task)| requeue_claim(&project_in_dir(root), verb, task))
        .count()
}

pub(super) fn hand_claims_to_live_successor(successor: &str) -> usize {
    let claims = snapshot_in_flight_claims();
    write_handoff_inherited_claims(successor, &claims);
    requeue_claims_for_live_successor(&claims)
}

pub(super) fn handoff_inherited_claims_path() -> PathBuf {
    install_dir().join("handoff-inherited-claims.json")
}

pub(super) const HANDOFF_INHERITED_CLAIMS_MAX_AGE_MS: u64 = 15 * 60 * 1000;

pub(super) fn write_handoff_inherited_claims(version: &str, claims: &[AbandonedClaim]) {
    let path = handoff_inherited_claims_path();
    if claims.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let payload = serde_json::json!({
        "version": version,
        "pid": std::process::id(),
        "ts": now_ms(),
        "claims": claims
            .iter()
            .map(|(root, verb, task)| serde_json::json!({
                "root": root.to_string_lossy(),
                "verb": verb,
                "task": task,
            }))
            .collect::<Vec<_>>(),
    });
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, payload.to_string());
}

pub(super) fn clear_handoff_inherited_claims() {
    let _ = fs::remove_file(handoff_inherited_claims_path());
}

pub(super) fn read_handoff_inherited_claims() -> HashSet<AbandonedClaim> {
    let Ok(raw) = fs::read_to_string(handoff_inherited_claims_path()) else {
        return HashSet::new();
    };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return HashSet::new();
    };
    let describes_a_handoff_still_in_progress = marker
        .get("ts")
        .and_then(|t| t.as_u64())
        .map(|ts| now_ms().saturating_sub(ts) <= HANDOFF_INHERITED_CLAIMS_MAX_AGE_MS)
        .unwrap_or(false);
    if !describes_a_handoff_still_in_progress {
        return HashSet::new();
    }
    marker
        .get("claims")
        .and_then(|c| c.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some((
                        PathBuf::from(row.get("root")?.as_str()?),
                        row.get("verb")?.as_str()?.to_string(),
                        row.get("task")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True when the dispatch holding a lane is still a live caller in some daemon.
///
/// This is the same question the orphan sweep asks of a claim file, asked of a lane holder: is the
/// dispatch still in this daemon's in-flight map, and if not, does some daemon with a live pid
/// still carry a fresh ledger row for it. A holder the daemon cannot name -- empty verb or empty
/// task -- is treated as live, because an unidentified holder must never be reaped: reaping a
/// running dispatch would let a second mutating git run in the same repo.
fn lane_holder_is_live(root: &Path, _lane: &'static str, verb: &str, task: &str) -> bool {
    if verb.is_empty() || task.is_empty() {
        return true;
    }
    if in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&(root.to_path_buf(), verb.to_string(), task.to_string()))
    {
        return true;
    }
    let spool_dir = spool_dir_of_root(root);
    live_dispatch_wait_owner(&spool_dir, verb, task).is_some()
}

/// True while a worker is registered for this dispatch, which is the runner's own liveness ledger.
///
/// The in-flight entry is inserted before the dispatch thread spawns and removed only when that
/// thread's scope ends, so it is present for exactly as long as a worker exists for the task. The
/// stage map is not: a worker that dies after noting `Running` leaves the stage behind, and the
/// stage alone then claims the lane is protecting a write that no longer exists.
fn lane_holder_worker_registered(key: &InFlightKey) -> bool {
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(key)
}

/// Why this lane holder must give the lane up, or `None` while it must keep it.
///
/// Four holds are pathological, and all are named so the ledger says which one fired:
///
/// - `holder_dispatch_gone`: nothing live owns the dispatch any more. It cannot be running, so the
///   hold is a tombstone and the lane is free.
/// - `holder_stuck_before_running`: the dispatch is live but its stage is not `Running`, and has not
///   been for `LANE_HOLD_STUCK_MIN_MS`. It holds the lane while it waits for its tool-queue turn, so
///   it has not touched the worktree; the lane is taken from it and the displaced-holder check stops
///   it from running on a lane it no longer owns.
/// - `holder_worker_not_alive`: the holder reached `Running`, but no worker is registered for it any
///   more and has not been for `LANE_HOLD_STUCK_MIN_MS`. The stage map is bookkeeping the worker
///   itself never clears when it dies, so a stage alone cannot say whether the lane still protects a
///   write: the in-flight registration is what the runner drops when the worker's thread ends, and
///   without it there is no writer left to protect.
/// - `holder_running_past_max`: the holder IS executing, and has been for
///   `LANE_HOLD_RUNNING_MAX_MS`, with claims waiting behind it. Execution that long is a hung
///   holder, not slow work, and an unbounded hold stalls the project's whole lane; see the constant.
///
/// A holder this daemon cannot see the stage of -- owned by another daemon -- is never released,
/// because there is no way to know whether it is mid-write.
pub(crate) fn lane_holder_stuck_reason(
    root: &Path,
    lane: &'static str,
    verb: &str,
    task: &str,
) -> Option<&'static str> {
    if verb.is_empty() || task.is_empty() {
        return None;
    }
    let key = (root.to_path_buf(), verb.to_string(), task.to_string());
    let record = dispatch_wait_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|record| (record.stage, record.stage_since_ms));
    // A staged holder is one this daemon is running, so the stage is authoritative: `Running` means
    // it is executing, and only an execution past the absolute ceiling takes the lane from it; any
    // other stage means it holds the lane without having touched the worktree, and past the bound
    // that hold is over.
    match record {
        Some((DispatchStage::Running, since_ms))
            if now_ms().saturating_sub(since_ms) >= lane_hold_stuck_min_ms()
                && !lane_holder_worker_registered(&key) =>
        {
            return Some("holder_worker_not_alive")
        }
        Some((DispatchStage::Running, since_ms))
            if now_ms().saturating_sub(since_ms) >= lane_hold_running_max_ms()
                && agentplug_host::registry::lane_waiter_count(root, lane) > 0 =>
        {
            return Some("holder_running_past_max")
        }
        Some((DispatchStage::Running, _)) => return None,
        Some((_, since_ms)) if now_ms().saturating_sub(since_ms) >= lane_hold_stuck_min_ms() => {
            return Some("holder_stuck_before_running")
        }
        Some(_) => return None,
        // Unstaged here: it may be mid-claim, or owned by another daemon. The only safe reason to
        // free the lane is that no daemon anywhere still owns this dispatch.
        None => {}
    }
    if in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        return None;
    }
    if live_dispatch_wait_owner(&spool_dir_of_root(root), verb, task).is_some() {
        return None;
    }
    Some("holder_dispatch_gone")
}

/// A lane hold past the stuck bound, released with its reason.
#[derive(Debug)]
pub(crate) struct ReleasedLaneHold {
    pub(crate) lane: &'static str,
    pub(crate) verb: String,
    pub(crate) task: String,
    pub(crate) held_ms: u64,
    pub(crate) reason: &'static str,
}

/// Frees every lane whose holder is gone, stuck before running, running past the ceiling with
/// claims behind it, or running with no worker left to run it.
///
/// This is the hold watchdog's action. A holder released as gone or stuck before running was not
/// executing, so the lane stays single-writer: the displaced holder asks `still_owns()` before
/// dispatching and answers starved instead of running. A holder released as running past the ceiling
/// IS executing -- that release is the lane draining instead of stalling, and it only fires past
/// `LANE_HOLD_RUNNING_MAX_MS` with waiters queued, so the single-writer property is spent only to
/// unblock somebody and only on a hold no real git operation reaches. A holder released as a worker
/// that is gone protects no write at all: nothing is executing, so the lane is free and the dispatch
/// is marked abandoned instead of being left to hold a lane and a claim forever.
pub(crate) fn release_stuck_lane_holders(min_held_ms: u64) -> Vec<ReleasedLaneHold> {
    let owners = agentplug_host::release_dead_lane_owners(min_held_ms, &|root, lane, verb, task| {
        lane_holder_stuck_reason(root, lane, verb, task).is_none()
    });
    owners
        .into_iter()
        .map(|owner| {
            let reason = lane_holder_stuck_reason(&owner.root, owner.lane, &owner.verb, &owner.task)
                .unwrap_or("holder_dispatch_gone");
            // A worker that is gone never answers its own dispatch, so the stage record that has the
            // ledger still naming it `claimed_running` is dropped here: the row becomes unowned, and
            // the orphan sweep reaps the claim and answers `dispatch_orphaned` instead of leaving
            // one dead dispatch in flight against the project cap forever.
            if reason == "holder_worker_not_alive" {
                forget_dispatch_wait_record(&owner.root, &owner.verb, &owner.task);
            }
            ReleasedLaneHold {
                lane: owner.lane,
                verb: owner.verb,
                task: owner.task,
                held_ms: owner.held_ms,
                reason,
            }
        })
        .collect()
}

fn lane_hold_stuck_min_ms() -> u64 {
    env_ms_or("AGENTPLUG_LANE_HOLD_STUCK_MIN_MS", LANE_HOLD_STUCK_MIN_MS_DEFAULT)
}

fn lane_hold_running_max_ms() -> u64 {
    env_ms_or(
        "AGENTPLUG_LANE_HOLD_RUNNING_MAX_MS",
        LANE_HOLD_RUNNING_MAX_MS_DEFAULT,
    )
}

pub(crate) fn release_abandoned_project_lanes() {
    let live_roots = read_registry();
    let min_held_ms = env_ms_or("AGENTPLUG_LANE_RELEASE_MIN_AGE_MS", MIN_ORPHAN_CLAIM_AGE_MS);
    for owner in agentplug_host::release_abandoned_lane_owners(&live_roots, min_held_ms) {
        eprintln!(
            "[agentplug daemon] released the {} lane of {} held by task {} for {}ms -- its requester has no live lease, so the lane no longer waits on it; the dispatch itself keeps running",
            owner.lane,
            owner.root.display(),
            owner.task,
            owner.held_ms
        );
    }
    for owner in agentplug_host::release_dead_lane_owners(min_held_ms, &lane_holder_is_live) {
        eprintln!(
            "[agentplug daemon] released the {} lane of {} -- holder {}/{} held it for {}ms and no dispatch in this daemon owns it and no live daemon ledger names it",
            owner.lane,
            owner.root.display(),
            owner.verb,
            owner.task,
            owner.held_ms
        );
    }
}

pub(super) const DISPATCH_WAIT_LEDGER_FRESH_MS: u64 = 120_000;
pub(super) const DURATION_SETTLE_GRACE_MS: u64 = 5_000;
pub(super) const WAIT_MAX_SERVED_MS: u64 = 600_000;

/// Ledger states that name NO owner: the writing daemon lists the file but keeps no dispatch record
/// for it, so the row restates the claim's existence instead of proving somebody runs it. A row in
/// one of these states is how a claim that outlived its daemon came to be protected by the very
/// ledger that lists it -- the sweep wrote the row, the row protected the claim, and the claim kept
/// counting against the project's in-flight cap forever (measured 2026-10-11 in C:/dev/spoint:
/// three `claimed_not_yet_tracked` rows at ~6.9e6 ms under a live daemon pid).
pub(super) const LEDGER_UNOWNED_STATES: [&str; 2] = ["claimed_not_yet_tracked", "never_claimed"];

/// A dispatch-wait ledger row is a live daemon's own statement that `(verb, task)` is in flight
/// for it. A sweep that owns no in-process entry for that key can still read it, so an owned
/// claim is never reaped as an orphan by a sweeper that cannot see the owner.
///
/// The statement counts only when the daemon that wrote it can name a dispatch: a row THIS daemon
/// wrote in an unowned state is this daemon saying it holds no record for the task, which is the
/// orphan case, not the owned one. A FOREIGN daemon's row still protects the claim whatever its
/// state -- that daemon may track the dispatch in ways this process cannot read, and the sweep
/// already refuses to run at all while a foreign sweeper holds a live heartbeat on this spool.
pub(super) fn live_dispatch_wait_owner(spool_dir: &Path, verb: &str, task: &str) -> Option<u64> {
    let raw = fs::read_to_string(spool_dir.join(DISPATCH_WAIT_LEDGER_FILE)).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let ts = value.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    if now_ms().saturating_sub(ts) >= DISPATCH_WAIT_LEDGER_FRESH_MS {
        return None;
    }
    let pid = value.get("daemon_pid").and_then(|p| p.as_u64())?;
    if !pid_is_alive(pid) {
        return None;
    }
    let written_by_this_daemon = pid == std::process::id() as u64;
    let rows = value.get("requests").and_then(|r| r.as_array())?;
    let owned = rows.iter().any(|row| {
        if row.get("verb").and_then(|v| v.as_str()) != Some(verb)
            || row.get("task").and_then(|t| t.as_str()) != Some(task)
        {
            return false;
        }
        let unowned_state = row
            .get("state")
            .and_then(|s| s.as_str())
            .map(|state| LEDGER_UNOWNED_STATES.contains(&state))
            .unwrap_or(false);
        !(written_by_this_daemon && unowned_state)
    });
    if owned {
        Some(pid)
    } else {
        None
    }
}

pub(super) fn verb_settles_by_requested_duration(verb: &str) -> bool {
    matches!(verb, "wait" | "sleep")
}

pub(super) fn requested_duration_ms(claim_path: &Path) -> Option<u64> {
    let raw = fs::read_to_string(claim_path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    for key in ["ms", "durationMs", "duration_ms"] {
        if let Some(entry) = value.get(key) {
            if let Some(ms) = entry.as_u64().or_else(|| entry.as_f64().map(|f| f as u64)) {
                return Some(ms);
            }
        }
    }
    value
        .get("body")
        .and_then(|body| body.get("ms"))
        .and_then(|entry| entry.as_u64().or_else(|| entry.as_f64().map(|f| f as u64)))
}

/// A `wait`/`sleep` claim whose requested duration has already elapsed has performed the wait it
/// asked for: settle it as completed, or as timed_out when it asked for longer than the verb
/// serves, so the caller gets a real ticket instead of an unverified orphan.
pub(super) fn settled_duration_outcome(
    claim_path: &Path,
    verb: &str,
    task: &str,
    claim_age: Option<u64>,
) -> Option<(serde_json::Value, String)> {
    if !verb_settles_by_requested_duration(verb) {
        return None;
    }
    let requested = requested_duration_ms(claim_path)?;
    let age = claim_age?;
    if age < requested.saturating_add(DURATION_SETTLE_GRACE_MS) {
        return None;
    }
    let timed_out = requested > WAIT_MAX_SERVED_MS;
    let settled_as = if timed_out { "timed_out" } else { "completed" };
    let waited_ms = if timed_out { age } else { requested };
    let body = serde_json::json!({
        "ok": !timed_out,
        "completed": !timed_out,
        "timed_out": timed_out,
        "waited_ms": waited_ms,
        "requested_ms": requested,
        "claim_age_ms": age,
        "data": {"completed": !timed_out, "waited_ms": waited_ms},
        "settled_by": "orphan-sweep",
        "settled_as": settled_as,
        "reason": if timed_out {
            "the claim asked for longer than the wait verb serves, so its caller timed out before the wait could complete"
        } else {
            "the claim's requested wait has already elapsed, so the wait it asked for has happened"
        },
        "verb": verb,
        "task": task,
        "sweeping_pid": std::process::id(),
    });
    let note = format!(
        "settled as {settled_as} (requested {requested}ms, claim age {age}ms, waited_ms {waited_ms}) -- no longer reported as an unverified orphan"
    );
    Some((body, note))
}

pub fn sweep_orphaned_claims(root: &Path) {
    sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &read_handoff_inherited_claims());
}

pub fn sweep_orphaned_claims_across_roots(roots: &[PathBuf]) {
    release_abandoned_project_lanes();
    let inherited = read_handoff_inherited_claims();
    for root in roots {
        sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &inherited);
    }
    clear_handoff_inherited_claims();
}

pub(super) const MIN_ORPHAN_CLAIM_AGE_MS: u64 = 60_000;

/// An absolute ceiling on how long a dispatch-wait ledger row may keep a claim alive. The row is
/// written by whichever daemon refreshed the ledger last, so it can name a daemon that never owned
/// the claim; past this age the row is no better evidence than the claim file itself, and the sweep
/// must be allowed to answer the caller. A claim whose owner beats its own marker is not held to
/// this: that marker is the owner's own statement, refreshed for as long as it runs.
pub(super) const ORPHAN_CLAIM_MAX_AGE_MS: u64 = 120_000;

pub(super) fn claim_age_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().ok()?.as_millis() as u64)
}

pub(super) fn sweep_orphaned_claims_distinguishing_handoff_from_crash(
    root: &Path,
    inherited: &HashSet<AbandonedClaim>,
) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    if fs::create_dir_all(&out_dir).is_err() {
        return;
    }
    if let Some(sweeper_pid) = live_foreign_spool_sweeper(&spool_dir) {
        if inherited.is_empty() {
            eprintln!(
                "[agentplug daemon] skipping orphan sweep for {} -- pid {sweeper_pid} holds a live heartbeat on this spool, so its in-flight claims are not orphans this process can see",
                root.display()
            );
            return;
        }
    }
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
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
            if inherited.contains(&(root.to_path_buf(), verb.clone(), task.clone())) {
                if requeue_claim(&in_dir, &verb, &task) {
                    eprintln!("[agentplug daemon] re-queued claim {verb}/{task} for {} -- a version handoff to a confirmed-ready successor abandoned it, so it is inherited work, not orphaned work; the caller waits longer and never sees dispatch_orphaned", root.display());
                    continue;
                }
                eprintln!("[agentplug daemon] could not re-queue handoff-inherited claim {verb}/{task} for {} -- falling through to dispatch_orphaned rather than swallowing it", root.display());
            }
            if in_flight_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&(root.to_path_buf(), verb.clone(), task.clone()))
            {
                continue;
            }
            if let Some((heartbeat_pid, heartbeat_age_ms)) =
                claim_heartbeat(&spool_dir, &verb, &task)
            {
                if heartbeat_age_ms
                    < env_ms_or("AGENTPLUG_CLAIM_HEARTBEAT_STALE_MS", CLAIM_HEARTBEAT_STALE_MS)
                {
                    eprintln!(
                        "[agentplug daemon] leaving claim {verb}/{task} for {} alone -- its owner renewed it {heartbeat_age_ms}ms ago under pid {heartbeat_pid}, so the dispatch that claimed it is still running however long it takes",
                        root.display()
                    );
                    continue;
                }
            }
            let claim_age = claim_age_ms(&path);
            let past_absolute_ceiling = claim_age
                .map(|age| {
                    age >= env_ms_or(
                        "AGENTPLUG_ORPHAN_CLAIM_MAX_AGE_MS",
                        ORPHAN_CLAIM_MAX_AGE_MS,
                    )
                })
                .unwrap_or(false);
            if !past_absolute_ceiling {
                if let Some(owner_pid) = live_dispatch_wait_owner(&spool_dir, &verb, &task) {
                    eprintln!(
                        "[agentplug daemon] leaving claim {verb}/{task} for {} alone -- the dispatch-wait ledger names it in flight under live daemon pid {owner_pid}, so it is owned work, not orphaned work",
                        root.display()
                    );
                    continue;
                }
            }
            if claim_age
                .map(|age| {
                    age < env_ms_or(
                        "AGENTPLUG_MIN_ORPHAN_CLAIM_AGE_MS",
                        MIN_ORPHAN_CLAIM_AGE_MS,
                    )
                })
                .unwrap_or(false)
            {
                continue;
            }
            if let Some((settled_body, settled_note)) =
                settled_duration_outcome(&path, &verb, &task, claim_age)
            {
                if write_spool_out_confirmed(&out_dir, &verb, &task, &settled_body.to_string()) {
                    eprintln!(
                        "[agentplug daemon] settled elapsed claim {verb}/{task} for {} -- {settled_note}",
                        root.display()
                    );
                    remove_claim_file(&spool_dir, &verb, &task, &path);
                } else {
                    eprintln!("[agentplug daemon] could not confirm the settled out-file for {verb}/{task} -- leaving the claim for the next sweep rather than deleting it unanswered");
                }
                continue;
            }
            let out_name = format!("{verb}-{task}.json");
            let out_confirmed = if out_dir.join(&out_name).exists() {
                true
            } else {
                let out_body = serde_json::json!({
                    "ok": false,
                    "error_code": "dispatch_orphaned",
                    "reaped": true,
                    "reason": "claim file with no live dispatch renewing it -- a dispatch that is still running renews its claim every 15s, so no owner was renewing this one; reaped by the periodic orphan sweep so it cannot hold the project busy",
                    "claim_age_ms": claim_age,
                    "claim_heartbeat_age_ms": claim_heartbeat(&spool_dir, &verb, &task)
                        .map(|(_, age)| age),
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that stopped answering -- a wasm trap, an out-of-memory abort, or a shared-Store recycle during the call. A version handoff is NOT a cause of this error: a handoff re-queues its claims for the incoming daemon, which completes them. The outcome is UNVERIFIED, not known to be unperformed: a side-effecting verb (git_commit/git_finalize/git_push/fs_write/memorize-fire) may already have applied some or all of its work, so read the real state (git log, git status, the file, the store) before re-dispatching. Re-dispatch straight away only for a read-only verb."),
                    "verb": verb,
                    "task": task,
                    "sweeping_pid": std::process::id(),
                }).to_string();
                let confirmed = write_spool_out_confirmed(&out_dir, &verb, &task, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
                confirmed
            };
            if out_confirmed {
                remove_claim_file(&spool_dir, &verb, &task, &path);
            } else {
                eprintln!("[agentplug daemon] could not confirm the dispatch_orphaned out-file for {verb}/{task} -- leaving the claim for the next sweep rather than deleting it unanswered");
            }
        }
    }
}

pub fn sweep_unconsumable_spool_files(root: &Path) {
    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let quarantine_dir = spool_dir.join("in-quarantine");
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return;
    };
    for verb_entry in verb_dirs.flatten() {
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if verb.is_empty() {
                continue;
            }
            let _ = fs::create_dir_all(&quarantine_dir);
            let dest = quarantine_dir.join(format!("_toplevel__{verb}"));
            if fs::rename(&verb_entry.path(), &dest).is_ok() {
                eprintln!(
                    "[agentplug daemon] quarantined stray file in/{verb} to {} -- the spool ABI puts every request in/<verb>/<file>, so a file sitting directly in in/ is never claimed by the dispatch loop and would otherwise sit invisibly forever{}",
                    dest.display(),
                    if verb.contains("${") { " (its name still holds an unexpanded shell template)" } else { "" }
                );
            }
            continue;
        }
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !file_entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if is_spool_request_path(&verb, &path) || ext == Some(ORPHAN_CLAIM_EXT) {
                continue;
            }
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name.is_empty() {
                continue;
            }
            let _ = fs::create_dir_all(&quarantine_dir);
            let dest = quarantine_dir.join(format!("{verb}__{file_name}"));
            if fs::rename(&path, &dest).is_ok() {
                eprintln!(
                    "[agentplug daemon] quarantined unconsumable spool file in/{verb}/{file_name} to {} -- the spool ABI is in/<verb>/<session-id>-<local-counter>.<ext>, so a non-conforming name is never claimed by the dispatch loop",
                    dest.display()
                );
                answer_quarantined_spool_request(&spool_dir.join("out"), &verb, &path, &dest);
            }
        }
    }
}

fn answer_quarantined_spool_request(out_dir: &Path, verb: &str, request_path: &Path, quarantined_to: &Path) {
    let file_name = request_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(task) = request_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let out_name = format!("{verb}-{task}.json");
    if out_dir.join(&out_name).exists() {
        return;
    }
    let mut accepted = accepted_spool_request_extensions(verb).to_vec();
    accepted.dedup();
    let accepted_form = format!(
        "in/{verb}/<session-id>-<counter>.<{}>",
        accepted.join("|")
    );
    let out_body = serde_json::json!({
        "ok": false,
        "verb": verb,
        "task": task,
        "error_code": "spool_filename_rejected",
        "error": format!(
            "request in/{verb}/{file_name} was never executed: its name is not an accepted spool request form. Write the request as {accepted_form}; a plain numeric name or another extension is quarantined, never claimed."
        ),
        "accepted_form": accepted_form,
        "accepted_extensions": accepted,
        "quarantined_to": quarantined_to.to_string_lossy(),
    })
    .to_string();
    let _ = fs::create_dir_all(out_dir);
    if !write_spool_out_confirmed(out_dir, &verb, &task, &out_body) {
        eprintln!("[agentplug daemon] could not write the spool_filename_rejected out-file for {verb}/{task}");
    }
}

pub(super) const SPOOL_OUT_REAP_INTERVAL_MS: u64 = 10 * 60 * 1000;
pub(super) const SPOOL_OUT_MIN_AGE_MS: u64 = 10 * 60 * 1000;
pub(super) const SPOOL_OUT_DEFAULT_MAX_FILES: usize = 3000;
pub(super) const SPOOL_OUT_DEFAULT_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
pub(super) const SPOOL_OUT_MAX_REAP_PER_PASS: usize = 1500;
/// An answered out-file is kept at least this long after it was written, whatever the file count: a
/// witness is cited against its dispatch id within two hours, and once the 500-entry dispatch ledger
/// has aged that id out, the out-file is the only copy of the answer.
pub(super) const SPOOL_OUT_WITNESS_HORIZON_MS: u64 = 2 * 60 * 60 * 1000;
/// Disk safety valve for a request flood: above this many out-files the oldest are trimmed even
/// inside the witness horizon.
pub(super) const SPOOL_OUT_HARD_MAX_FILES: usize = 20_000;

pub(super) fn spool_out_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(super) fn spool_out_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(super) fn spool_out_max_files() -> usize {
    spool_out_env_usize("GM_SPOOL_OUT_MAX_FILES", SPOOL_OUT_DEFAULT_MAX_FILES)
}

pub(super) fn spool_out_max_age_ms() -> u64 {
    spool_out_env_u64("GM_SPOOL_OUT_MAX_AGE_MS", SPOOL_OUT_DEFAULT_MAX_AGE_MS)
}

pub(super) fn spool_out_reap_timestamps() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn out_file_age_ms(metadata: &fs::Metadata, now: u64) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .map(|modified_ms| now.saturating_sub(modified_ms))
        .unwrap_or(0)
}

pub(super) fn out_meta_stamp_ms(metadata: &fs::Metadata, now: u64) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(now)
}

pub(super) fn remove_out_file_and_markers(path: &Path) {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(parent) = path.parent() {
        if !file_name.is_empty() {
            let mut ready = file_name.clone();
            ready.push_str(".ready");
            let _ = fs::remove_file(parent.join(&ready));
        }
        if !stem.is_empty() {
            let _ = fs::remove_file(parent.join(format!("{stem}.txt")));
        }
    }
    let _ = fs::remove_file(path);
}

pub(super) fn live_out_stems(root: &Path) -> HashSet<String> {
    let in_dir = spool_dir_of_root(root).join("in");
    let mut live: HashSet<String> = HashSet::new();
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return live;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            if !file_entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let file_name = file_entry.file_name().to_string_lossy().into_owned();
            let without_claim = file_name
                .strip_suffix(&format!(".{ORPHAN_CLAIM_EXT}"))
                .unwrap_or(file_name.as_str());
            let task = without_claim
                .rsplit_once('.')
                .map(|(head, _)| head)
                .unwrap_or(without_claim);
            if !task.is_empty() {
                live.insert(format!("{verb}-{task}"));
            }
        }
    }
    live
}

pub fn reap_spool_out_files(root: &Path, force: bool) -> usize {
    let spool_dir = spool_dir_of_root(root);
    let out_dir = spool_dir.join("out");
    let now = now_ms();
    if !force {
        let mut last = spool_out_reap_timestamps()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(last.get(root).copied().unwrap_or(0)) < SPOOL_OUT_REAP_INTERVAL_MS {
            return 0;
        }
        last.insert(root.to_path_buf(), now);
    }
    let Ok(entries) = fs::read_dir(&out_dir) else {
        return 0;
    };
    let live = live_out_stems(root);

    struct OutRow {
        path: PathBuf,
        stem: String,
        stamp_ms: u64,
    }
    let mut rows: Vec<OutRow> = Vec::new();
    let mut total_entries = 0usize;
    let mut stale_tmp = 0usize;
    let mut json_names: HashSet<String> = HashSet::new();
    let mut ready_markers: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        total_entries += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".tmp.") {
            let age_ms = entry
                .metadata()
                .ok()
                .map(|m| out_file_age_ms(&m, now))
                .unwrap_or(0);
            if age_ms >= SPOOL_OUT_MIN_AGE_MS && fs::remove_file(entry.path()).is_ok() {
                stale_tmp += 1;
            }
            continue;
        }
        if let Some(answer_name) = name.strip_suffix(".ready") {
            if answer_name.ends_with(".json") {
                ready_markers.push((entry.path(), answer_name.to_string()));
            }
            continue;
        }
        if !name.ends_with(".json") {
            continue;
        }
        json_names.insert(name.clone());
        let stem = entry
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stamp_ms = entry
            .metadata()
            .ok()
            .map(|m| out_meta_stamp_ms(&m, now))
            .unwrap_or(now);
        rows.push(OutRow {
            path: entry.path(),
            stem,
            stamp_ms,
        });
    }

    rows.sort_by_key(|row| row.stamp_ms);
    let max_files = spool_out_max_files();
    let max_age_ms = spool_out_max_age_ms();
    let over_cap = rows.len().saturating_sub(max_files);
    let mut orphan_markers = 0usize;
    for (marker_path, answer_name) in &ready_markers {
        if orphan_markers >= SPOOL_OUT_MAX_REAP_PER_PASS {
            break;
        }
        if json_names.contains(answer_name) {
            continue;
        }
        let age_ms = fs::metadata(marker_path)
            .ok()
            .map(|m| out_file_age_ms(&m, now))
            .unwrap_or(0);
        if age_ms >= SPOOL_OUT_MIN_AGE_MS && fs::remove_file(marker_path).is_ok() {
            orphan_markers += 1;
        }
    }
    let hard_excess = rows.len().saturating_sub(SPOOL_OUT_HARD_MAX_FILES);
    let mut reaped = 0usize;
    for (index, row) in rows.iter().enumerate() {
        if reaped >= SPOOL_OUT_MAX_REAP_PER_PASS {
            break;
        }
        let age_ms = now.saturating_sub(row.stamp_ms);
        if age_ms < SPOOL_OUT_MIN_AGE_MS {
            break;
        }
        if live.contains(&row.stem) {
            continue;
        }
        let past_horizon = age_ms >= SPOOL_OUT_WITNESS_HORIZON_MS;
        let over_count_cap = past_horizon && index < over_cap;
        let expired_by_age = past_horizon && age_ms >= max_age_ms;
        let over_hard_cap = index < hard_excess;
        if !(over_count_cap || expired_by_age || over_hard_cap) {
            continue;
        }
        remove_out_file_and_markers(&row.path);
        reaped += 1;
    }
    if reaped > 0 || stale_tmp > 0 || orphan_markers > 0 {
        eprintln!(
            "[agentplug daemon] reaped {} answered out-file(s), {} orphaned .ready marker(s) and {} stale tmp file(s) under {} -- out/ had {} entries ({} json), kept at least {}h, capped at {} files past that horizon (hard ceiling {}), {} dispatches still live",
            reaped,
            orphan_markers,
            stale_tmp,
            out_dir.display(),
            total_entries,
            rows.len(),
            SPOOL_OUT_WITNESS_HORIZON_MS / (60 * 60 * 1000),
            max_files,
            SPOOL_OUT_HARD_MAX_FILES,
            live.len()
        );
    }
    reaped
}

pub(super) const RAW_PLUGIN_SPOOL_VERBS: &[&str] = &["libsql", "bert"];

pub(super) fn extract_session_id(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    ["session_id", "sessionId", "SESSION_ID"]
        .into_iter()
        .filter_map(|name| value.get(name).and_then(|entry| entry.as_str()))
        .map(str::trim)
        .find(|session_id| !session_id.is_empty())
        .map(str::to_string)
}

pub(super) fn session_id_task_mismatch_rejection(verb: &str, task: &str, body: &str) -> Option<String> {
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

pub(super) static LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn last_measured_dispatch_queue_wait_ms() -> u64 {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) const ADMISSION_ANNOTATION_MAX_BYTES: usize = 1024 * 1024;

pub(crate) fn annotate_admission_on_reply(
    out_body: String,
    admission: Option<agentplug_host::AdmissionRecord>,
) -> String {
    let Some(record) = admission else {
        return out_body;
    };
    if out_body.len() > ADMISSION_ANNOTATION_MAX_BYTES {
        return out_body;
    }
    let Ok(mut reply) = serde_json::from_str::<serde_json::Value>(&out_body) else {
        return out_body;
    };
    let Some(fields) = reply.as_object_mut() else {
        return out_body;
    };
    fields.insert("admission_decision".to_string(), serde_json::json!("admitted"));
    fields.insert(
        "admission_band".to_string(),
        serde_json::json!(record.band.as_str()),
    );
    fields.insert(
        "admission_waited_ms".to_string(),
        serde_json::json!(record.waited_ms),
    );
    reply.to_string()
}

pub(super) fn admission_starved_answer(
    root: &Path,
    verb: &str,
    task: &str,
    lane: Option<&'static str>,
    kind: &'static str,
    waited_ms: u64,
    limit: usize,
    in_flight: usize,
    slots: usize,
    holders: &str,
) -> String {
    let spool_dir = spool_dir_of_root(root);
    let (request_path, claim_path, out_path) = unanswered_spool_paths(&spool_dir, verb, task);
    serde_json::json!({
        "ok": false,
        "error_code": "dispatch_starved_waiting_for_admission",
        "verb": verb,
        "task": task,
        "lane": lane,
        "admission_kind": kind,
        "admission_in_flight": in_flight,
        "admission_limit": limit,
        "plugin_slots": slots,
        "admission_holders": holders,
        "waited_ms": waited_ms,
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "error": format!(
            "verb {} (task {}) was claimed by daemon pid {} but never entered the {} admission gate within {}ms -- {} dispatches already hold the {} admitted slots of {} (holders, oldest first: {}), so it was NOT executed and re-dispatching is safe. Live state for every unanswered request is in {}.",
            verb, task, std::process::id(), kind, waited_ms, in_flight, limit, slots, holders,
            spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).display()
        ),
    })
    .to_string()
}

pub(super) fn unanswered_spool_paths(spool_dir: &Path, verb: &str, task: &str) -> (PathBuf, PathBuf, PathBuf) {
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    let request_path = in_dir.join(verb).join(format!("{task}.txt"));
    let claim_path = inflight_claim_path(&in_dir, verb, task);
    let out_path = out_dir.join(format!("{verb}-{task}.json"));
    (request_path, claim_path, out_path)
}

pub(super) fn spool_dir_of_root(root: &Path) -> PathBuf {
    root.join(".gm").join("exec-spool")
}

pub(crate) fn lane_wait_max(verb: &str, caller_budget_ms: Option<u64>) -> Duration {
    let bounded = match caller_budget_ms {
        Some(budget) => lane_wait_ceiling_ms(verb).min(budget.max(LANE_WAIT_FLOOR_MS)),
        None => lane_wait_default_ms(verb),
    };
    Duration::from_millis(bounded)
}

/// The wait a caller that stated no budget gets: short for a verb whose whole point is a fast
/// answer, the lane ceiling everywhere else.
pub(crate) fn lane_wait_default_ms(verb: &str) -> u64 {
    if verb == "codesearch" {
        return env_ms_or(
            "AGENTPLUG_CODESEARCH_LANE_WAIT_MS",
            CODESEARCH_LANE_WAIT_MS_DEFAULT,
        );
    }
    lane_wait_ceiling_ms(verb)
}

/// The longest wait a caller may buy on this lane, before the caller's own budget clamps it.
///
/// A default that doubled as the ceiling is what answered a `timeout_seconds: 240` caller at 5008ms:
/// a caller that named a budget was clipped to the short default instead of buying the ceiling. The
/// two are separate numbers now, so the prompt default is kept and the ceiling stays buyable.
pub(crate) fn lane_wait_ceiling_ms(verb: &str) -> u64 {
    if dispatch_serial_lane(verb, "") == Some("git") {
        return git_lane_wait_max().as_millis() as u64;
    }
    env_ms_or("AGENTPLUG_LANE_WAIT_MAX_MS", LANE_WAIT_MAX_MS_DEFAULT)
}

/// Splits the caller's lane-wait budget out of a dispatch body.
///
/// Returns the body with the member removed -- that is the body the plugin sees and the one hashed
/// for the claim's FIFO identity, so the channel is invisible to both -- plus the budget it carried.
/// The reserved member gm-mcp writes from `timeout_seconds` wins; `lane_wait_ms` is the public
/// spelling for a caller writing its own body. A body that is not a JSON object, or that carries the
/// name without a plain integer value, is returned untouched with no budget: an unparseable channel
/// is ignored rather than acted on.
pub(crate) fn split_caller_lane_wait_ms(body: &str) -> (String, Option<u64>) {
    let (stripped, reserved) = strip_lane_wait_member(body, LANE_WAIT_MS_BODY_KEY);
    if reserved.is_some() {
        return (stripped, reserved);
    }
    let (stripped_public, public) = strip_lane_wait_member(&stripped, LANE_WAIT_MS_PUBLIC_BODY_KEY);
    (stripped_public, public)
}

fn strip_lane_wait_member(body: &str, member: &str) -> (String, Option<u64>) {
    let bytes = body.as_bytes();
    let key_at_body = format!("\"{}\"", member);
    let key = key_at_body.as_bytes();
    let Some(key_at) = bytes.windows(key.len()).position(|window| window == key) else {
        return (body.to_string(), None);
    };
    let is_space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\r' | b'\n');
    let mut at = key_at + key.len();
    while at < bytes.len() && is_space(bytes[at]) {
        at += 1;
    }
    if at >= bytes.len() || bytes[at] != b':' {
        return (body.to_string(), None);
    }
    at += 1;
    while at < bytes.len() && is_space(bytes[at]) {
        at += 1;
    }
    let digits_at = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if at == digits_at {
        return (body.to_string(), None);
    }
    let budget = body[digits_at..at].parse::<u64>().ok();
    let value_end = at;
    // Drop the member and one separating comma, whichever side it sits on, so every other byte of
    // the body is preserved exactly.
    let mut after = value_end;
    while after < bytes.len() && is_space(bytes[after]) {
        after += 1;
    }
    if after < bytes.len() && bytes[after] == b',' {
        return (
            format!("{}{}", &body[..key_at], &body[after + 1..]),
            budget,
        );
    }
    let mut before = key_at;
    while before > 0 && is_space(bytes[before - 1]) {
        before -= 1;
    }
    if before > 0 && bytes[before - 1] == b',' {
        return (
            format!("{}{}", &body[..before - 1], &body[value_end..]),
            budget,
        );
    }
    (
        format!("{}{}", &body[..key_at], &body[value_end..]),
        budget,
    )
}

/// The wait budget for a dispatch that lands on the git serial lane.
///
/// Every lane budget exists to turn an unreachable lane into an answer the caller can act on, not
/// to cancel work: a starved dispatch never ran, so `re_dispatch_safe` holds no matter how long
/// the budget is. Git is the slowest lane by a wide margin -- `git add`/`git commit` over a
/// multi-thousand-path index takes minutes on this box -- and every starved caller re-dispatches
/// immediately, so a short budget does not shorten anybody's wait, it only multiplies the number
/// of waiters competing for the same lane. One long wait beats a dozen short ones.
pub(super) fn git_lane_wait_max() -> Duration {
    Duration::from_millis(env_ms_or(
        "AGENTPLUG_GIT_LANE_WAIT_MAX_MS",
        GIT_LANE_WAIT_MAX_MS_DEFAULT,
    ))
}


/// `granted_ms` is the budget this dispatch was actually given, so the answer names the wait it
/// lost to and the ceiling it could have bought instead of leaving the caller to guess one.
pub(super) fn lane_starved_answer(
    root: &Path,
    verb: &str,
    task: &str,
    lane: Option<&'static str>,
    report: &LaneWaitReport,
    granted_ms: u64,
) -> String {
    let ceiling_ms = lane_wait_ceiling_ms(verb);
    let running_max_ms = lane_hold_running_max_ms();
    let releases_in_ms = running_max_ms.saturating_sub(report.lane_holder_age_ms);
    let release_in = if releases_in_ms > 0 {
        format!("the lane frees in {releases_in_ms}ms")
    } else {
        "the holder is already past it and the next watchdog tick frees the lane".to_string()
    };
    let escape_timing = format!(
        "Past {running_max_ms}ms of executing, the hold watchdog releases this holder with reason holder_running_past_max so the lane drains: {release_in}. Re-dispatching queues behind the same holder, so when the holder is hung the route that reaches the remote is the GitHub Git API publish in escape_hatch, not another dispatch."
    );
    let spool_dir = spool_dir_of_root(root);
    let (request_path, claim_path, out_path) = unanswered_spool_paths(&spool_dir, verb, task);
    serde_json::json!({
        "ok": false,
        "error_code": "dispatch_starved_waiting_for_serial_lane",
        "verb": verb,
        "task": task,
        "lane": lane,
        "waited_ms": report.waited_ms,
        "lane_waiters": report.waiters,
        "lane_position": report.lane_position,
        "lane_holder_task": report.lane_holder_task,
        "lane_holder_verb": report.lane_holder_verb,
        "lane_holder_held_ms": report.lane_holder_held_ms,
        "lane_holder_age_ms": report.lane_holder_age_ms,
        "lane_wait_budget_ms": granted_ms,
        "lane_wait_default_ms": lane_wait_default_ms(verb),
        "lane_wait_ceiling_ms": ceiling_ms,
        "suggested_lane_wait_ms": ceiling_ms,
        "lane_hold_running_max_ms": running_max_ms,
        "lane_hold_releases_in_ms": releases_in_ms,
        "escape_hatch": "this lane is a serialization point, not the only way to publish: to route around a holder instead of re-queueing behind it, publish through the GitHub Git API -- POST /repos/<owner>/<repo>/git/blobs (base64, CRLF normalized to LF), POST /repos/<owner>/<repo>/git/trees with base_tree set to the remote tip, POST /repos/<owner>/<repo>/git/commits with parents:[tip], then PATCH /repos/<owner>/<repo>/git/refs/heads/<branch> with {\"sha\":<commit>,\"force\":false} -- and verify with GET /repos/<owner>/<repo>/contents/<path>?ref=<branch> whose blob sha equals git hash-object of the file.",
        "retry_hint": format!(
            "re-dispatch this verb with \"lane_wait_ms\": {ceiling_ms} (or timeout_seconds: {}) to wait for the lane instead of being answered at {granted_ms}ms",
            ceiling_ms.div_ceil(1_000)
        ),
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "re_dispatch_keeps_lane_place": true,
        "error": format!(
            "verb {} (task {}) was claimed by daemon pid {} but never reached the {} serial lane within {}ms, so it was NOT executed -- re-dispatching is safe because nothing ran. The lane is held by one dispatch at a time per project (git, store, state lanes) and admits waiters oldest-claim-first; holder task {} (verb {}, holding the lane for {}ms), this dispatch was {} in the lane queue with {} waiter(s) behind it. Re-dispatching this verb with a byte-identical body re-claims the place this dispatch already held -- a lane place is keyed on the verb plus a hash of the body, never on the task id -- so the retry keeps its FIFO position; resuming this task id only re-reads this answer. Live state for every unanswered request is in {}. {}",
            verb, task, std::process::id(), lane.unwrap_or("unassigned"), report.waited_ms,
            report.lane_holder_task.as_deref().unwrap_or("unknown"),
              report.lane_holder_verb.as_deref().unwrap_or("unknown"),
              report.lane_holder_age_ms,
              report.lane_position.map(|position| position.to_string()).unwrap_or_else(|| "unknown".to_string()),
              report.waiters,
            spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).display(),
            escape_timing
        ),
    })
    .to_string()
}

/// The answer for a dispatch the hold watchdog displaced from a lane it had already won.
///
/// It holds the lane and has not run, so the reply is the same shape as a starvation: nothing
/// executed, re-dispatching is safe, and the claim keeps its place at the head of the queue. The
/// extra fields name the release, because this is the one starvation the caller did not cause by
/// running out of budget -- the lane was taken from a holder that was stuck, and the caller's
/// re-dispatch is the continuation of the same request, not a new one.
pub(super) fn lane_displaced_answer(root: &Path, verb: &str, task: &str, lane: Option<&'static str>) -> String {
    let spool_dir = spool_dir_of_root(root);
    let (request_path, claim_path, out_path) = unanswered_spool_paths(&spool_dir, verb, task);
    serde_json::json!({
        "ok": false,
        "error_code": "dispatch_starved_waiting_for_serial_lane",
        "verb": verb,
        "task": task,
        "lane": lane,
        "lane_hold_released": true,
        "lane_hold_released_reason": "holder_stuck_before_running",
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "error": format!(
            "verb {} (task {}) won the {:?} serial lane but the hold watchdog released that hold before it ran, so it was NOT executed -- re-dispatching is safe because nothing ran. The lane stays single-writer: a holder that lost it never touches the worktree. This claim is parked at the head of its lane queue, so the re-dispatch keeps its place. Live state for every unanswered request is in {}.",
            verb, task, lane.unwrap_or("unassigned"),
            spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).display()
        ),
    })
    .to_string()
}

pub(super) fn tool_queue_starved_answer(
    root: &Path,
    verb: &str,
    task: &str,
    lane: Option<&'static str>,
    report: &ToolQueueWaitReport,
) -> String {
    let spool_dir = spool_dir_of_root(root);
    let (request_path, claim_path, out_path) = unanswered_spool_paths(&spool_dir, verb, task);
    let queue_key = report.queue_key.replace('\u{0}', "/");
    serde_json::json!({
        "ok": false,
        "error_code": "dispatch_starved_waiting_for_tool_queue",
        "verb": verb,
        "task": task,
        "lane": lane,
        "tool_queue": queue_key,
        "queue_position": report.position,
        "queue_holder_task": report.holder_task,
        "waited_ms": report.waited_ms,
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "error": format!(
            "verb {} (task {}) was claimed by daemon pid {} but never reached the front of the {} queue within {}ms, so it was NOT executed -- re-dispatching is safe because nothing ran. That queue admits one dispatch at a time per project and per verb; holder task {}. Live state for every unanswered request is in {}.",
            verb, task, std::process::id(), queue_key, report.waited_ms,
            report.holder_task.as_deref().unwrap_or("unknown"),
            spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).display()
        ),
    })
    .to_string()
}

pub(crate) fn run_gm_dispatch_to_file(
    root: &Path,
    handle: &DispatchHandle,
    verb: &str,
    task: &str,
    body: &str,
    out_dir: &Path,
    queue_wait_ms: u64,
    submitted_at_ms: Option<u64>,
) {
    LAST_MEASURED_DISPATCH_QUEUE_WAIT_MS.store(queue_wait_ms, std::sync::atomic::Ordering::Relaxed);
    let plugin_name = if RAW_PLUGIN_SPOOL_VERBS.contains(&verb) {
        verb
    } else {
        "gm"
    };
    let inner_verb_owned: String = if plugin_name == "gm" {
        String::new()
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("verb")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "capabilities".to_string())
    };
    let tool_verb = if plugin_name == "gm" {
        verb
    } else {
        inner_verb_owned.as_str()
    };
    // The caller's own poll budget rides in the body as one reserved member (gm-mcp writes it from
    // `timeout_seconds`). Take it out here, before anything hashes or runs the body: the plugin below
    // must see exactly the body the caller wrote, and the lane's FIFO identity is hashed from that
    // body, so a member that appeared in one dispatch and not in its re-dispatch would fork one
    // waiting request into two queue places. The budget itself is what bounds the lane wait -- the
    // caller could not raise it before, so a caller polling for 240s starved at a hard 120s.
    let (body_owned, caller_lane_wait_ms) = split_caller_lane_wait_ms(body);
    let body = body_owned.as_str();
    let lane = dispatch_serial_lane(tool_verb, body);
    let lane_wait_budget = lane_wait_max(tool_verb, caller_lane_wait_ms);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    // Renew this dispatch's claim for as long as it runs: a wait {"ms":120000} was answered
    // dispatch_orphaned at 64s because an untouched claim reads exactly like an abandoned one, so a
    // bounded wait could be reaped inside its own budget by a sweep that could not see the owner.
    // The guard covers the lane wait too, not only the execution.
    let _claim_heartbeat = ClaimHeartbeat::start(&in_dir, verb, task);
    note_dispatch_stage(root, verb, task, DispatchStage::WaitingForLane, lane);
    let _fairness_guard = match GmFairnessGuard::acquire_within(
        root,
        tool_verb,
        body,
        task,
        lane_wait_budget,
    ) {
        Ok(guard) => guard,
        Err(report) => {
            let out_body = lane_starved_answer(
                root,
                verb,
                task,
                lane,
                &report,
                lane_wait_budget.as_millis() as u64,
            );
            eprintln!(
                "[agentplug daemon] {verb}/{task} for {} was claimed but never reached the {:?} lane within {}ms -- answered dispatch_starved_waiting_for_serial_lane; the verb did not run",
                root.display(), lane, report.waited_ms
            );
            write_spool_out_and_release_claim(out_dir, &in_dir, verb, task, &out_body);
            forget_dispatch_wait_record(root, verb, task);
            refresh_dispatch_wait_ledger(root);
            return;
        }
    };
    let _store_owner = if lane == Some("store") {
        match settle_store_lock(root) {
            StoreLockState::Held { lock_dir, owner_pid, detail } => {
                eprintln!(
                    "[agentplug daemon] {verb}/{task} for {} refused: {detail}",
                    root.display()
                );
                let out_body = serde_json::json!({
                    "ok": false,
                    "verb": verb,
                    "error_code": "store_busy",
                    "store_busy": true,
                    "lock_dir": lock_dir.display().to_string(),
                    "lock_owner_pid": owner_pid,
                    "error": detail,
                })
                .to_string();
                write_spool_out_and_release_claim(out_dir, &in_dir, verb, task, &out_body);
                forget_dispatch_wait_record(root, verb, task);
                refresh_dispatch_wait_ledger(root);
                return;
            }
            StoreLockState::Clear | StoreLockState::Reclaimed => Some(StoreOwnerRecord::claim(root)),
        }
    } else if agentplug_host::dispatch_claims_store_owner(tool_verb) {
        // Unserialized store writers (the codeinsight family) never reach settle_store_lock, so they
        // must not settle -- a held lock there is answered by the guest from its persisted index, not
        // refused -- but they do own the lock while they write, and claiming it is what lets the next
        // settle tell a live writer from a directory an unclean exit left behind.
        Some(StoreOwnerRecord::claim(root))
    } else {
        None
    };
    note_dispatch_stage(root, verb, task, DispatchStage::WaitingForToolQueue, lane);
    let _tool_guard = match ToolDispatchGuard::acquire_within(
        root,
        plugin_name,
        tool_verb,
        body,
        task,
        lane_wait_budget,
    ) {
        Ok(guard) => guard,
        Err(report) => {
            let out_body = tool_queue_starved_answer(root, verb, task, lane, &report);
            eprintln!(
                "[agentplug daemon] {verb}/{task} for {} was claimed but never reached the front of queue {} within {}ms -- answered dispatch_starved_waiting_for_tool_queue; the verb did not run",
                root.display(), report.queue_key.replace('\u{0}', "/"), report.waited_ms
            );
            write_spool_out_and_release_claim(out_dir, &in_dir, verb, task, &out_body);
            forget_dispatch_wait_record(root, verb, task);
            refresh_dispatch_wait_ledger(root);
            return;
        }
    };
    // A hold is bounded, so the watchdog may have taken this lane while we waited for the tool
    // queue: it releases a holder that is stuck *before* running, and such a holder is not executing.
    // Ask before dispatching rather than after -- a guard that no longer owns the lane must never
    // reach the worktree, or two mutating git dispatches would share one repo. The claim is parked
    // back at the head of its queue, so the caller's re-dispatch keeps the place it earned instead of
    // rejoining behind every newer arrival.
    if !_fairness_guard.still_owns() {
        eprintln!(
            "[agentplug daemon] {verb}/{task} for {} won the {:?} lane but lost it to the hold watchdog before running -- answered dispatch_starved_waiting_for_serial_lane and parked the claim at the head of the queue; the verb did not run",
            root.display(), lane
        );
        agentplug_host::requeue_lane_claimer_front(root, lane, tool_verb, body, task);
        let out_body = lane_displaced_answer(root, verb, task, lane);
        write_spool_out_and_release_claim(out_dir, &in_dir, verb, task, &out_body);
        forget_dispatch_wait_record(root, verb, task);
        refresh_dispatch_wait_ledger(root);
        return;
    }
    note_dispatch_stage(root, verb, task, DispatchStage::Running, lane);
    let _dispatch_origin_scope =
        agentplug_host::enter_dispatch_origin_scope(task, submitted_at_ms);
    let dispatch_result = if plugin_name == "gm" {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.dispatch("gm", verb, body)
        }))
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.dispatch(plugin_name, &inner_verb_owned, body)
        }))
    };
    let admission = agentplug_host::take_last_admission();
    let out_body = match dispatch_result {
        Ok(Ok(s)) if !s.is_empty() => s,
        Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
        Ok(Err(e)) => match e.downcast_ref::<agentplug_host::PluginDispatchError>() {
            Some(agentplug_host::PluginDispatchError::AdmissionStarved { kind, waited_ms, limit, in_flight, slots, holders }) => {
                eprintln!(
                    "[agentplug daemon] {verb}/{task} for {} was claimed but never entered the {kind} admission gate within {waited_ms}ms (holders: {holders}) -- answered dispatch_starved_waiting_for_admission; the verb did not run",
                    root.display()
                );
                admission_starved_answer(root, verb, task, lane, *kind, *waited_ms, *limit, *in_flight, *slots, holders)
            }
            _ => serde_json::json!({"ok": false, "error": describe_dispatch_error_naming_wasm_trap_kind_distinctly_from_a_guest_logic_error(&e), "verb": verb}).to_string(),
        },
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
    let out_body = annotate_admission_on_reply(out_body, admission);
    let out_body = patch_update_available_from_escalation(plugin_name, verb, out_body);
    let out_body = spill_large_exec_output_to_text_sibling(out_dir, verb, task, out_body);
    let out_confirmed = write_spool_out_confirmed(out_dir, verb, task, &out_body);
    if out_confirmed {
        let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
    } else {
        eprintln!("[agentplug daemon] out-file write for {verb}/{task} did not confirm for {} -- leaving the claim for the orphan sweep instead of deleting an unanswered request", root.display());
    }
    // The result is written, so this dispatch is finished: no admission slot may stay
    // booked under this thread. Under unwinding the guards already released and this is
    // a no-op; a slot whose guard never ran is reclaimed here instead of starving the
    // next dispatch.
    let reclaimed = agentplug_host::release_admission_for_current_thread();
    if reclaimed > 0 {
        eprintln!(
            "[agentplug daemon] {verb}/{task} for {} reclaimed {reclaimed} admission slot(s) still booked to its thread after its result was written",
            root.display()
        );
    }
    let key: InFlightKey = (root.to_path_buf(), verb.to_string(), task.to_string());
    in_flight_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    forget_dispatch_wait_record(root, verb, task);
    refresh_dispatch_wait_ledger(root);
}

pub(super) const DISPATCH_WAIT_LEDGER_STALE_MS: u64 = 60_000;

pub(super) fn dispatch_wait_ledger_is_mine_or_stale(ledger_path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(ledger_path) else {
        return true;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return true;
    };
    let mine = value
        .get("daemon_pid")
        .and_then(|p| p.as_u64())
        .map(|pid| pid == std::process::id() as u64)
        .unwrap_or(true);
    let stale = value
        .get("ts")
        .and_then(|t| t.as_u64())
        .map(|ts| now_ms().saturating_sub(ts) > DISPATCH_WAIT_LEDGER_STALE_MS)
        .unwrap_or(true);
    mine || stale
}

pub(super) const CODE_INDEX_CONSUMING_VERBS: &[&str] = &[
    "codesearch",
    "codeinsight",
    "codeinsight_callers",
    "codeinsight_index",
    "search",
];

#[derive(Clone)]
pub(super) struct CodeIndexState {
    cold: bool,
    partial: bool,
    digest: Option<String>,
}

pub(super) fn code_index_state(spool_dir: &Path) -> CodeIndexState {
    let Ok(raw) = fs::read_to_string(spool_dir.join(".codeinsight-digest")) else {
        return CodeIndexState {
            cold: true,
            partial: false,
            digest: None,
        };
    };
    let digest = raw.trim().to_string();
    if digest.is_empty() {
        return CodeIndexState {
            cold: true,
            partial: false,
            digest: None,
        };
    }
    let deferred = digest
        .rsplit_once(":partial=")
        .and_then(|(_, tail)| tail.split(':').next())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    CodeIndexState {
        cold: false,
        partial: deferred > 0,
        digest: Some(digest),
    }
}

/// True at most once per `LANE_HOLD_REPORT_INTERVAL_MS`, so a stalled holder is named on a readable
/// cadence instead of on every ledger refresh.
fn lane_hold_watchdog_due(now: u64) -> bool {
    static LAST_REPORT_MS: OnceLock<Mutex<u64>> = OnceLock::new();
    let mut last = LAST_REPORT_MS
        .get_or_init(|| Mutex::new(0))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if now.saturating_sub(*last) < LANE_HOLD_REPORT_INTERVAL_MS {
        return false;
    }
    *last = now;
    true
}

/// Bounded well inside `DISPATCH_WAIT_LEDGER_STALE_MS` so a quiet ledger still answers with a fresh `ts`.
const DISPATCH_WAIT_LEDGER_REFRESH: Duration = Duration::from_secs(15);

fn without_wait_volatile_fields(value: &serde_json::Value) -> serde_json::Value {
    let mut copy = value.clone();
    let Some(map) = copy.as_object_mut() else {
        return copy;
    };
    map.remove("ts");
    for row in map
        .get_mut("requests")
        .and_then(serde_json::Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(row) = row.as_object_mut() {
            for key in ["file_age_ms", "stage_age_ms", "lane_holder_age_ms"] {
                row.remove(key);
            }
        }
    }
    for list in ["lane_holds", "lane_hold_released"] {
        for hold in map
            .get_mut(list)
            .and_then(serde_json::Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(hold) = hold.as_object_mut() {
                hold.remove("held_ms");
            }
        }
    }
    copy
}

/// A ledger whose rows have not changed is left on disk: only the daemon pid, a real change, or the
/// liveness refresh forces another tmp-then-rename across every watched root.
fn dispatch_wait_ledger_write_needed(ledger_path: &Path, payload: &str) -> bool {
    let Ok(existing) = fs::read_to_string(ledger_path) else {
        return true;
    };
    let (Ok(next), Ok(previous)) = (
        serde_json::from_str::<serde_json::Value>(payload),
        serde_json::from_str::<serde_json::Value>(&existing),
    ) else {
        return true;
    };
    if previous.get("daemon_pid") != next.get("daemon_pid") {
        return true;
    }
    let age = fs::metadata(ledger_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .unwrap_or(Duration::MAX);
    if age >= DISPATCH_WAIT_LEDGER_REFRESH {
        return true;
    }
    without_wait_volatile_fields(&previous) != without_wait_volatile_fields(&next)
}

pub(super) fn refresh_dispatch_wait_ledger(root: &Path) {
    let spool_dir = spool_dir_of_root(root);
    let ledger_path = spool_dir.join(DISPATCH_WAIT_LEDGER_FILE);
    let (queued, claimed) = spool_step_counts(root);
    let root_has_waits = dispatch_wait_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .any(|(active_root, _, _)| active_root == root);
    if !root_has_waits && queued == 0 && claimed == 0 {
        if ledger_path.exists() && dispatch_wait_ledger_is_mine_or_stale(&ledger_path) {
            let _ = fs::remove_file(&ledger_path);
        }
        return;
    }
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");
    let Ok(verb_dirs) = fs::read_dir(&in_dir) else {
        return;
    };
    let now = now_ms();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut index_state: Option<CodeIndexState> = None;
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(verb_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            let name = file_entry.file_name().to_string_lossy().into_owned();
            let is_claim = name.ends_with(&format!(".{}", ORPHAN_CLAIM_EXT));
            let is_queued_request = !is_claim && is_spool_request_path(&verb, &path);
            if !is_claim && !is_queued_request {
                continue;
            }
            let task = if is_claim {
                Path::new(path.file_stem().unwrap_or_default())
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            if task.is_empty() || out_dir.join(format!("{verb}-{task}.json")).exists() {
                continue;
            }
            let modified_ms = file_entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(now);
            let (request_path, claim_path, out_path) =
                unanswered_spool_paths(&spool_dir, &verb, &task);
            let record = dispatch_wait_record(root, &verb, &task);
            let admission_wait = record
                .as_ref()
                .and_then(|record| agentplug_host::admission_wait_state_for_thread(record.thread));
            let state = match (&record, &admission_wait) {
                (_, Some(_)) => "claimed_waiting_for_admission",
                (Some(record), None) => dispatch_stage_name(record.stage),
                (None, None) if is_claim => "claimed_not_yet_tracked",
                (None, None) => "never_claimed",
            };
            let stage_age_ms = admission_wait
                .as_ref()
                .map(|wait| now.saturating_sub(wait.since_ms))
                .or_else(|| {
                    record
                        .as_ref()
                        .map(|record| now.saturating_sub(record.stage_since_ms))
                })
                .unwrap_or(0);
            let lane = record.as_ref().and_then(|record| record.lane);
            let queue_state = agentplug_host::registry::lane_queue_state(root, lane, &task);
            let index_state = if CODE_INDEX_CONSUMING_VERBS.contains(&verb.as_str()) {
                Some(index_state.get_or_insert_with(|| code_index_state(&spool_dir)).clone())
            } else {
                None
            };
            rows.push(serde_json::json!({
                "verb": verb,
                "task": task,
                "state": state,
                "lane": lane,
                "lane_position": queue_state.lane_position,
                "lane_holder_age_ms": queue_state.lane_holder_age_ms,
                "lane_waiters": queue_state.lane_waiters,
                "lane_holder_task": queue_state.lane_holder_task,
                "lane_holder_verb": queue_state.lane_holder_verb,
                "admission_kind": admission_wait.as_ref().map(|wait| wait.kind),
                "admission_in_flight": admission_wait.as_ref().map(|wait| wait.in_flight),
                "admission_limit": admission_wait.as_ref().map(|wait| wait.limit),
                "file_age_ms": now.saturating_sub(modified_ms),
                "stage_age_ms": stage_age_ms,
                "index_cold": index_state.as_ref().map(|state| state.cold),
                "index_partial": index_state.as_ref().map(|state| state.partial),
                "index_digest": index_state.as_ref().and_then(|state| state.digest.clone()),
                "request_path": request_path.to_string_lossy(),
                "claim_path": if is_claim { claim_path.to_string_lossy().into_owned() } else { String::new() },
                "out_path": out_path.to_string_lossy(),
            }));
        }
    }
    if rows.is_empty() {
        let _ = fs::remove_file(&ledger_path);
        return;
    }
    let lane_holds = agentplug_host::registry::lane_holds(root);
    // A hold past the bound is now bounded, not merely reported: the watchdog frees the lane when the
    // holder is gone, or is live but has sat at a pre-Running stage past the stuck bound -- neither is
    // executing, so the displaced holder asks `still_owns()` before dispatching and answers starved
    // instead of running on a lane it no longer owns -- and, past the running ceiling with waiters
    // queued, it frees the lane even from a holder that IS executing. That last case is the one an
    // unbounded hold produces: a holder that never finishes stalls every later claim on the lane, and
    // the waiters it releases are told to re-dispatch, which only queues them behind the same holder.
    let released_holds = if lane_hold_watchdog_due(now) {
        release_stuck_lane_holders(lane_hold_stuck_min_ms())
    } else {
        Vec::new()
    };
    let lane_hold_rows: Vec<serde_json::Value> = lane_holds
        .iter()
        .map(|hold| {
            let released = released_holds
                .iter()
                .find(|released| {
                    released.lane == hold.lane
                        && released.verb == hold.verb
                        && released.task == hold.task
                })
                .map(|released| released.reason);
            serde_json::json!({
                "lane": hold.lane,
                "verb": hold.verb,
                "task": hold.task,
                "held_ms": hold.held_ms,
                "waiters": hold.waiters,
                "holds_waiters": hold.waiters > 0,
                "over_bound": hold.held_ms >= LANE_HOLD_REPORT_MIN_MS,
                "released": released.is_some(),
                "released_reason": released,
            })
        })
        .collect();
    for hold in &lane_holds {
        if hold.held_ms >= LANE_HOLD_REPORT_MIN_MS && lane_hold_watchdog_due(now) {
            let released = released_holds.iter().find(|released| {
                released.lane == hold.lane && released.verb == hold.verb && released.task == hold.task
            });
            match released {
                Some(released) => eprintln!(
                    "[lane-hold] project {} lane {} held by task {} (verb {}) for {}ms with {} waiter(s) -- past the {}ms bound, released as {}: {}",
                    root.display(),
                    hold.lane,
                    hold.task,
                    hold.verb,
                    hold.held_ms,
                    hold.waiters,
                    LANE_HOLD_REPORT_MIN_MS,
                    released.reason,
                    if released.reason == "holder_running_past_max" {
                        format!(
                            "the holder IS executing but has run past the {}ms ceiling with waiters behind it, so the lane was freed rather than left stalled -- its dispatch keeps running and no longer owns the lane, and the waiters stop being told to re-queue behind it",
                            lane_hold_running_max_ms()
                        )
                    } else if released.reason == "holder_worker_not_alive" {
                        "no worker is registered for the holder any more, so the lane protected no write: it was freed and the dispatch is marked abandoned for the orphan sweep to answer, instead of holding the lane and a claim for as long as the daemon lives".to_string()
                    } else {
                        "the holder was not executing, so the lane was freed and the holder's own still_owns() check answers starved instead of running it. Still one writer per lane.".to_string()
                    }
                ),
                None => eprintln!(
                    "[lane-hold] project {} lane {} held by task {} (verb {}) for {}ms with {} waiter(s) -- past the {}ms bound and NOT released: the holder is running, or its stage is another daemon's to know. One writer per lane is kept.",
                    root.display(),
                    hold.lane,
                    hold.task,
                    hold.verb,
                    hold.held_ms,
                    hold.waiters,
                    LANE_HOLD_REPORT_MIN_MS
                ),
            }
        }
    }
    let payload = serde_json::json!({
        "daemon_pid": std::process::id(),
        "ts": now,
        "root": root.to_string_lossy(),
        "project_in_flight": project_in_flight_count(root),
        "project_in_flight_cap": MAX_CLAIMED_DISPATCHES_PER_PROJECT,
        "lane_hold_report_min_ms": LANE_HOLD_REPORT_MIN_MS,
        "lane_hold_stuck_min_ms": lane_hold_stuck_min_ms(),
        "lane_hold_running_max_ms": lane_hold_running_max_ms(),
        "lane_hold_released": released_holds
            .iter()
            .map(|released| {
                serde_json::json!({
                    "lane": released.lane,
                    "verb": released.verb,
                    "task": released.task,
                    "held_ms": released.held_ms,
                    "reason": released.reason,
                })
            })
            .collect::<Vec<serde_json::Value>>(),
        "lane_holds": lane_hold_rows,
        "requests": rows,
    })
    .to_string();
    if !dispatch_wait_ledger_write_needed(&ledger_path, &payload) {
        return;
    }
    let tmp = spool_dir.join(format!(
        "{DISPATCH_WAIT_LEDGER_FILE}.tmp.{}",
        std::process::id()
    ));
    if fs::write(&tmp, payload).is_ok() {
        let _ = fs::rename(&tmp, &ledger_path);
    }
}

pub(super) fn dir_has_any_verb_subdir_with_claimable_request(base: &Path, language_stems: bool) -> bool {
    let Ok(verb_dirs) = fs::read_dir(base) else {
        return false;
    };
    for verb_entry in verb_dirs.flatten() {
        if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let verb = verb_entry.file_name().to_string_lossy().into_owned();
        if verb_dir_has_claimable_request(&verb_entry.path(), &verb, language_stems) {
            return true;
        }
    }
    false
}

pub(super) const EMPTY_VERB_DIR_VERDICT_MIN_AGE: Duration = Duration::from_secs(2);

pub(super) fn empty_verb_dir_verdicts() -> &'static Mutex<HashMap<PathBuf, std::time::SystemTime>> {
    static SLOT: OnceLock<Mutex<HashMap<PathBuf, std::time::SystemTime>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn verb_dir_has_claimable_request(verb_dir: &Path, verb: &str, language_stems: bool) -> bool {
    let modified = fs::metadata(verb_dir)
        .and_then(|metadata| metadata.modified())
        .ok();
    if let Some(modified) = modified {
        let unchanged_since_empty_verdict = empty_verb_dir_verdicts()
            .lock()
            .ok()
            .is_some_and(|verdicts| verdicts.get(verb_dir) == Some(&modified));
        if unchanged_since_empty_verdict {
            return false;
        }
    }
    let Ok(files) = fs::read_dir(verb_dir) else {
        return false;
    };
    for file_entry in files.flatten() {
        let path = file_entry.path();
        let claimable = if language_stems {
            is_spool_request_path(verb, &path)
        } else {
            path.extension().and_then(|extension| extension.to_str()) == Some("txt")
        };
        if claimable {
            return true;
        }
    }
    if let Some(modified) = modified {
        let settled = std::time::SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= EMPTY_VERB_DIR_VERDICT_MIN_AGE);
        if settled {
            if let Ok(mut verdicts) = empty_verb_dir_verdicts().lock() {
                verdicts.insert(verb_dir.to_path_buf(), modified);
            }
        }
    }
    false
}

#[cfg(windows)]
fn spool_inbox_dirs_of(root: &Path) -> [PathBuf; 2] {
    [
        spool_dir_of(root).join("in"),
        root.join(".agentplug").join("plugin-dispatch").join("in"),
    ]
}

#[cfg(windows)]
pub(super) struct IdleInDirWatch {
    entries: Vec<(PathBuf, PathBuf, windows_sys::Win32::Foundation::HANDLE)>,
    unwatched: Vec<PathBuf>,
    wanted_dirs: Vec<PathBuf>,
}

#[cfg(windows)]
impl IdleInDirWatch {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            unwatched: Vec::new(),
            wanted_dirs: Vec::new(),
        }
    }

    fn close_all(&mut self) {
        use windows_sys::Win32::Storage::FileSystem::FindCloseChangeNotification;
        for (_, _, handle) in self.entries.drain(..) {
            unsafe {
                FindCloseChangeNotification(handle);
            }
        }
    }

    fn sync(&mut self, roots: &[PathBuf]) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FindFirstChangeNotificationW, FILE_NOTIFY_CHANGE_DIR_NAME,
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SIZE,
        };
        let wanted: Vec<(PathBuf, PathBuf)> = roots
            .iter()
            .flat_map(|root| {
                spool_inbox_dirs_of(root)
                    .into_iter()
                    .map(move |dir| (root.clone(), dir))
            })
            .filter(|(_, dir)| dir.is_dir())
            .collect();
        if self.wanted_dirs.iter().eq(wanted.iter().map(|(_, dir)| dir)) {
            return;
        }
        self.close_all();
        self.unwatched.clear();
        self.wanted_dirs = wanted.iter().map(|(_, dir)| dir.clone()).collect();
        for (root, dir) in wanted {
            let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
            wide.push(0);
            let handle = unsafe {
                FindFirstChangeNotificationW(
                    wide.as_ptr(),
                    1,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE,
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                mark_spool_dirty(&root);
                self.entries.push((root, dir, handle));
            } else {
                self.unwatched.push(root);
            }
        }
    }

    const WAIT_CHUNK_HANDLES: usize = 64;

    fn wait(&self, cap: Duration) -> Vec<PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::FindNextChangeNotification;
        use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
        if self.entries.is_empty() {
            std::thread::sleep(cap);
            return Vec::new();
        }
        const WAIT_OBJECT_0: u32 = 0;
        let chunks: Vec<&[(PathBuf, PathBuf, windows_sys::Win32::Foundation::HANDLE)]> =
            self.entries.chunks(Self::WAIT_CHUNK_HANDLES).collect();
        let per_chunk_ms = (cap.as_millis() as u64 / chunks.len() as u64).max(1);
        let deadline = Instant::now() + cap;
        let mut changed: Vec<PathBuf> = Vec::new();
        for chunk in chunks {
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            if remaining_ms == 0 {
                break;
            }
            let handles: Vec<_> = chunk.iter().map(|(_, _, h)| *h).collect();
            let mut signaled: Vec<usize> = Vec::new();
            loop {
                let remaining_ms = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64;
                if remaining_ms == 0 {
                    break;
                }
                let rc = unsafe {
                    WaitForMultipleObjects(
                        handles.len() as u32,
                        handles.as_ptr(),
                        0,
                        per_chunk_ms.min(remaining_ms) as u32,
                    )
                };
                let Some(idx) = (WAIT_OBJECT_0..WAIT_OBJECT_0 + handles.len() as u32)
                    .contains(&rc)
                    .then(|| (rc - WAIT_OBJECT_0) as usize)
                else {
                    break;
                };
                if let Some((_, _, handle)) = chunk.get(idx) {
                    unsafe {
                        FindNextChangeNotification(*handle);
                    }
                }
                signaled.push(idx);
            }
            for idx in signaled {
                if let Some((root, _, _)) = chunk.get(idx) {
                    changed.push(root.clone());
                }
            }
        }
        changed
    }
}

#[cfg(windows)]
impl Drop for IdleInDirWatch {
    fn drop(&mut self) {
        self.close_all();
    }
}

#[cfg(windows)]
pub(super) fn wait_for_in_dir_change(watch: &mut IdleInDirWatch, roots: &[PathBuf], cap: Duration) {
    watch.sync(roots);
    for unwatched_root in &watch.unwatched {
        mark_spool_dirty(unwatched_root);
    }
    for changed_root in watch.wait(cap) {
        mark_spool_dirty(&changed_root);
    }
}

pub(super) fn project_has_queued_spool_work(root: &Path) -> bool {
    let pd_in = root.join(".agentplug").join("plugin-dispatch").join("in");
    if let Ok(plugin_dirs) = fs::read_dir(&pd_in) {
        for plugin_entry in plugin_dirs.flatten() {
            if !plugin_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            if dir_has_any_verb_subdir_with_claimable_request(&plugin_entry.path(), false) {
                return true;
            }
        }
    }
    let gm_in = root.join(".gm").join("exec-spool").join("in");
    dir_has_any_verb_subdir_with_claimable_request(&gm_in, true)
}

pub(super) fn project_has_queued_spool_work_cached(root: &Path) -> bool {
    cached_root_scan(root).0
}

pub(super) fn project_has_pending_dispatch_work(root: &Path) -> bool {
    project_in_flight_count(root) < MAX_CLAIMED_DISPATCHES_PER_PROJECT
        && project_has_queued_spool_work_cached(root)
}

pub(super) fn dispatch_project(
    root: &Path,
    project: &mut ProjectPlugins,
    plugin_modules: &PluginModules,
) -> bool {
    let mut did_work = false;
    if cfg!(not(windows)) {
        mark_spool_dirty(root);
    }

    let spool_dir = root.join(".gm").join("exec-spool");
    let in_dir = spool_dir.join("in");
    let out_dir = spool_dir.join("out");

    struct ClaimedRequest {
        verb: String,
        task: String,
        body: String,
        submitted_at_ms: Option<u64>,
    }
    let mut claimed: Vec<ClaimedRequest> = Vec::new();
    let queued_now = project_has_queued_spool_work_cached(root);
    let in_dir_scan = queued_now.then(|| fs::read_dir(&in_dir));
    let in_dir_existed = match &in_dir_scan {
        Some(scan) => scan.is_ok(),
        None => root_in_dir_exists_cached(root),
    };
    let mut claimable: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    if let Some(Ok(entries)) = in_dir_scan {
        for verb_entry in entries.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(verb_entry.path()) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if !is_spool_request_path(&verb, &file_path) {
                    continue;
                }
                if !spool_in_file_write_has_settled(&file_path) {
                    continue;
                }
                let queued_since = file_entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                claimable.push((queued_since, verb.clone(), file_path));
            }
        }
    }
    claimable.sort_by_key(|(queued_since, _, _)| *queued_since);
    let claim_budget =
        MAX_CLAIMED_DISPATCHES_PER_PROJECT.saturating_sub(project_in_flight_count(root));
    for (queued_since, verb, file_path) in claimable.into_iter().take(claim_budget) {
        let Some(claim_path) = spool_claim_path(&file_path) else {
            continue;
        };
        if fs::rename(&file_path, &claim_path).is_err() {
            continue;
        }
        let task = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let body = fs::read_to_string(&claim_path).unwrap_or_default();
        if body.trim().is_empty() {
            let _ = fs::rename(&claim_path, &file_path);
            continue;
        }
        in_flight_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (root.to_path_buf(), verb.clone(), task.clone()),
                InFlightHandle {
                    detach: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
            );
        let submitted_at_ms = queued_since
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since_epoch| since_epoch.as_millis() as u64)
            .filter(|ms| *ms > 0);
        did_work = true;
        claimed.push(ClaimedRequest {
            verb,
            task,
            body,
            submitted_at_ms,
        });
    }

    if !claimed.is_empty() {
        mark_spool_dirty(root);
    }
    if claimed.is_empty() && in_dir_existed && !project_has_pending_dispatch_work(root) {
        return did_work;
    }

    if !in_dir_existed {
        mark_spool_dirty(root);
    }
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return did_work;
    }
    write_project_heartbeat_rate_limited(root, read_status_busy_until_if_future(root), None);

    let requested_plugins = {
        let mut list = vec!["gm".to_string()];
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
    let mut lease_requests: Vec<ClaimedRequest> = Vec::new();
    for req in claimed {
        if let Some(out_body) = session_id_task_mismatch_rejection(&req.verb, &req.task, &req.body)
        {
            write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
            continue;
        }
        if req.verb == "background-convert" {
            bg_convert_requests.push(req);
        } else if req.verb == "plugin-refresh" {
            plugin_refresh_requests.push(req);
        } else if req.verb.starts_with("browser_lease_") {
            lease_requests.push(req);
        } else {
            gm_requests.push(req);
        }
    }
    for req in lease_requests {
        let out_body = agentplug_host::lease_reply(&req.verb, root, &req.body);
        write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
        did_work = true;
    }

    let answer_bg_converts = |reqs: Vec<ClaimedRequest>| {
        for req in reqs {
            let out_body = handle_background_convert(root, &req.body);
            write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
        }
    };
    for req in plugin_refresh_requests {
        let out_body = handle_plugin_refresh_request(root, &req.body);
        write_spool_out_and_release_claim(&out_dir, &in_dir, &req.verb, &req.task, &out_body);
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
                if plugin_name == "gm" {
                    gm_load_failure_reason = Some(reason);
                }
                continue;
            };
            if let Err(e) =
                project.load_plugin(&plugin_modules.engine, plugin_name, module, content_hash)
            {
                let reason = format!(
                    "failed to instantiate plugin {plugin_name} for {}: {e:#}",
                    root.display()
                );
                eprintln!("[agentplug daemon] {reason}");
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
                if plugin_name == "gm" {
                    gm_load_failure_reason = Some(reason);
                }
            }
        }

        if !project.is_loaded("gm") {
            let error_message = match &gm_load_failure_reason {
                Some(reason) => format!("gm plugin failed to load for this project: {reason}"),
                None => "gm plugin failed to load for this project (see daemon stderr for the compile/install/instantiate failure)".to_string(),
            };
            for req in &gm_requests {
                let out_body =
                    serde_json::json!({"ok": false, "error": error_message, "verb": req.verb})
                        .to_string();
                write_spool_out_and_release_claim(
                    &out_dir, &in_dir, &req.verb, &req.task, &out_body,
                );
            }
            answer_bg_converts(bg_convert_requests);
        } else {
            for req in gm_requests {
                let self_healing_dispatch_handle = project.dispatch_handle_with_reload(Some((
                    plugin_modules.engine.clone(),
                    plugin_modules.modules_with_hashes(),
                )));
                let detach_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let key: InFlightKey = (root.to_path_buf(), req.verb.clone(), req.task.clone());
                let failed_spawn_key = key.clone();
                let failed_spawn_verb = req.verb.clone();
                let failed_spawn_task = req.task.clone();
                in_flight_map()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        key.clone(),
                        InFlightHandle {
                            detach: detach_flag,
                        },
                    );

                let thread_root = root.to_path_buf();
                let thread_out_dir = out_dir.clone();
                let queue_wait_ms = req
                    .submitted_at_ms
                    .map(|submitted| now_ms().saturating_sub(submitted))
                    .unwrap_or(0);
                let spawn_result = std::thread::Builder::new()
                    .name(format!("gm-dispatch-{}", req.task))
                    .spawn(move || {
                        let _release_in_flight_entry = InFlightEntryRelease { key };
                        run_gm_dispatch_to_file(
                            &thread_root,
                            &self_healing_dispatch_handle,
                            &req.verb,
                            &req.task,
                            &req.body,
                            &thread_out_dir,
                            queue_wait_ms,
                            req.submitted_at_ms,
                        );
                    });
                if let Err(e) = spawn_result {
                    eprintln!("[agentplug daemon] could not spawn a dispatch thread for {}: {e} -- answering the request with an error instead of leaving it claimed", root.display());
                    in_flight_map()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&failed_spawn_key);
                    let out_body = serde_json::json!({"ok": false, "error": format!("daemon could not start a dispatch thread: {e}"), "verb": failed_spawn_verb}).to_string();
                    write_spool_out_and_release_claim(
                        &out_dir,
                        &in_dir,
                        &failed_spawn_verb,
                        &failed_spawn_task,
                        &out_body,
                    );
                }
            }

            answer_bg_converts(bg_convert_requests);
            write_project_heartbeat(root, Some(now_ms() + TICKER_BUSY_UNTIL_EXTEND_MS));
        }
    }

    if did_work {
        return true;
    }

    let pd_dir = root.join(".agentplug").join("plugin-dispatch");
    let pd_in = pd_dir.join("in");
    let pd_out = pd_dir.join("out");
    if fs::create_dir_all(&pd_in).is_err() || fs::create_dir_all(&pd_out).is_err() {
        return did_work;
    }
    let Ok(plugin_dirs) = fs::read_dir(&pd_in) else {
        return did_work;
    };
    for plugin_entry in plugin_dirs.flatten() {
        if !plugin_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let plugin_name = plugin_entry.file_name().to_string_lossy().into_owned();
        let Ok(verb_dirs) = fs::read_dir(plugin_entry.path()) else {
            continue;
        };
        for verb_entry in verb_dirs.flatten() {
            if !verb_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let verb = verb_entry.file_name().to_string_lossy().into_owned();
            let Ok(files) = fs::read_dir(verb_entry.path()) else {
                continue;
            };
            for file_entry in files.flatten() {
                let file_path = file_entry.path();
                if file_path.extension().and_then(|e| e.to_str()) != Some("txt") {
                    continue;
                }
                if !spool_in_file_write_has_settled(&file_path) {
                    continue;
                }
                let claim_path = plugin_dispatch_claim_path(&file_path);
                if fs::rename(&file_path, &claim_path).is_err() {
                    continue;
                }
                let task = file_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let body = fs::read_to_string(&claim_path).unwrap_or_default();

                let write_pd_out = |out_name: &str, out_body: &str| {
                    let out_body = stamp_dispatch_id(&pd_out, &verb, &task, out_body);
                    let tmp = pd_out.join(format!("{out_name}.tmp.{}", std::process::id()));
                    if fs::write(&tmp, out_body).is_ok() {
                        let _ = fs::rename(&tmp, pd_out.join(out_name));
                        let _ = fs::write(pd_out.join(format!("{out_name}.ready")), b"");
                    }
                    let _ = fs::remove_file(&claim_path);
                };

                {
                    let current = plugin_modules
                        .module_with_hash(&plugin_name)
                        .map(|(_, hash)| project.is_loaded_current(&plugin_name, hash))
                        .unwrap_or_else(|| project.is_loaded(&plugin_name));
                    if !current {
                        let Some((module, content_hash)) =
                            plugin_modules.module_with_hash(&plugin_name)
                        else {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin {plugin_name} not compiled yet for this daemon -- retry shortly")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        };
                        if let Err(e) = project.load_plugin(
                            &plugin_modules.engine,
                            &plugin_name,
                            module,
                            content_hash,
                        ) {
                            let out_name = format!("{plugin_name}-{verb}-{task}.json");
                            let out_body = serde_json::json!({"ok": false, "error": format!("plugin instantiate failed: {e:#}")}).to_string();
                            write_pd_out(&out_name, &out_body);
                            return true;
                        }
                    }
                }

                if let Some(reason) = shared_store_recycle_reason_independent_of_daemon_idle_state(
                    &DaemonConfig::load(),
                ) {
                    let mut released: Vec<&str> = Vec::new();
                    for shared_name in agentplug_host::RELEASABLE_SHARED_PLUGINS {
                        if shared_name != plugin_name
                            && agentplug_host::release_shared_plugin(shared_name)
                        {
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

                let _tool_guard = ToolDispatchGuard::acquire(root, &plugin_name, &verb, &body);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    project.dispatch(&plugin_name, &verb, &body)
                }));
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
                let out_body =
                    patch_update_available_from_escalation(&plugin_name, &verb, out_body);
                write_pd_out(&out_name, &out_body);
                return true;
            }
        }
    }

    did_work
}

pub enum DaemonDispatchOutcome {
    Answered(String),
    NeverClaimedRunLocally,
    ClaimedUnanswered(String),
}

pub(super) const PLUGIN_DISPATCH_CLAIM_WAIT_MS_DEFAULT: u64 = 30_000;
pub(super) const PLUGIN_DISPATCH_CLAIMED_TIMEOUT_MS_DEFAULT: u64 = 20 * 60 * 1000;
pub(super) const PLUGIN_DISPATCH_POLL_MS: u64 = 25;
pub(super) const PLUGIN_DISPATCH_OWNER_LIVENESS_CHECK_MS: u64 = 5_000;

pub(super) fn env_ms_or(name: &str, default_ms: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_ms)
}

pub(super) fn plugin_dispatch_claim_path(req_path: &Path) -> PathBuf {
    req_path.with_extension(format!("txt.claim.{}", std::process::id()))
}

pub(super) fn find_plugin_dispatch_claim(in_dir: &Path, task: &str) -> Option<(PathBuf, Option<u64>)> {
    let prefix = format!("{task}.txt.claim.");
    fs::read_dir(in_dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let claimer_pid = name.strip_prefix(&prefix)?.parse::<u64>().ok();
        Some((entry.path(), claimer_pid))
    })
}

pub(super) fn take_plugin_dispatch_answer(out_path: &Path) -> Option<String> {
    let content = fs::read_to_string(out_path).ok()?;
    let _ = fs::remove_file(out_path);
    let _ = fs::remove_file(out_path.with_extension("json.ready"));
    Some(content)
}

pub(super) fn reclaim_unclaimed_plugin_dispatch(req_path: &Path) -> bool {
    let reclaimed = req_path.with_extension(format!("txt.reclaimed.{}", std::process::id()));
    if fs::rename(req_path, &reclaimed).is_err() {
        return false;
    }
    let _ = fs::remove_file(&reclaimed);
    true
}

pub(super) fn unanswered_dispatch_report(
    error_code: &str,
    error: String,
    plugin: &str,
    verb: &str,
    task: &str,
    claimer_pid: Option<u64>,
    waited_ms: u64,
    out_path: &Path,
) -> String {
    serde_json::json!({
        "ok": false,
        "error_code": error_code,
        "error": error,
        "plugin": plugin,
        "verb": verb,
        "task": task,
        "claimer_pid": claimer_pid,
        "waited_ms": waited_ms,
        "out_path": out_path.to_string_lossy(),
        "re_executed_locally": false,
    })
    .to_string()
}

pub fn try_dispatch_via_daemon(
    cwd: &Path,
    plugin: &str,
    verb: &str,
    body: &str,
) -> DaemonDispatchOutcome {
    use DaemonDispatchOutcome::{Answered, ClaimedUnanswered, NeverClaimedRunLocally};
    let cwd = agentplug_host::project_root(cwd);
    if std::env::var("AGENTPLUG_NO_DAEMON").is_ok() {
        return NeverClaimedRunLocally;
    }
    if let Err(e) = register_project(&cwd) {
        eprintln!("[agentplug] {e}");
        return NeverClaimedRunLocally;
    }
    if !ensure_daemon_running().unwrap_or(false) {
        return NeverClaimedRunLocally;
    }

    let pd_dir = cwd.join(".agentplug").join("plugin-dispatch");
    let in_dir = pd_dir.join("in").join(plugin).join(verb);
    let out_dir = pd_dir.join("out");
    if fs::create_dir_all(&in_dir).is_err() || fs::create_dir_all(&out_dir).is_err() {
        return NeverClaimedRunLocally;
    }

    let task = format!("{}{}", std::process::id(), now_ms());
    let req_path = in_dir.join(format!("{task}.txt"));
    let staging_path = in_dir.join(format!("{task}.txt.staging"));
    if fs::write(&staging_path, body).is_err() || fs::rename(&staging_path, &req_path).is_err() {
        let _ = fs::remove_file(&staging_path);
        return NeverClaimedRunLocally;
    }
    let out_path = out_dir.join(format!("{plugin}-{verb}-{task}.json"));

    let claim_wait_ms = env_ms_or(
        "AGENTPLUG_DISPATCH_CLAIM_WAIT_MS",
        PLUGIN_DISPATCH_CLAIM_WAIT_MS_DEFAULT,
    );
    let claimed_timeout_ms = env_ms_or(
        "AGENTPLUG_DISPATCH_CLAIMED_TIMEOUT_MS",
        PLUGIN_DISPATCH_CLAIMED_TIMEOUT_MS_DEFAULT,
    );
    let started = Instant::now();
    let mut claimed_at: Option<Instant> = None;
    let mut last_owner_liveness_check = Instant::now();
    loop {
        if let Some(answer) = take_plugin_dispatch_answer(&out_path) {
            return Answered(answer);
        }
        let waited_ms = started.elapsed().as_millis() as u64;
        match claimed_at {
            None if !req_path.exists() => {
                claimed_at = Some(Instant::now());
                continue;
            }
            None => {
                if waited_ms >= claim_wait_ms
                    && find_plugin_dispatch_claim(&in_dir, &task).is_none()
                    && reclaim_unclaimed_plugin_dispatch(&req_path)
                {
                    eprintln!("[agentplug] daemon never claimed {plugin}/{verb} task {task} within {claim_wait_ms}ms -- reclaimed the request atomically, running it locally exactly once");
                    return NeverClaimedRunLocally;
                }
                if waited_ms >= claim_wait_ms.saturating_add(claimed_timeout_ms) {
                    return ClaimedUnanswered(unanswered_dispatch_report(
                        "fallback_reclaim_failed",
                        format!("{plugin}/{verb} task {task} was never claimed by the daemon, but the request file {} could not be atomically reclaimed for the local fallback within {waited_ms}ms -- not run locally, because the daemon could still claim it and the verb would then run twice", req_path.display()),
                        plugin, verb, task.as_str(), None, waited_ms, &out_path,
                    ));
                }
            }
            Some(at) => {
                if last_owner_liveness_check.elapsed()
                    >= Duration::from_millis(PLUGIN_DISPATCH_OWNER_LIVENESS_CHECK_MS)
                {
                    last_owner_liveness_check = Instant::now();
                    if let Some((claim_path, Some(claimer_pid))) =
                        find_plugin_dispatch_claim(&in_dir, &task)
                    {
                        if !pid_is_alive(claimer_pid) {
                            if let Some(answer) = take_plugin_dispatch_answer(&out_path) {
                                return Answered(answer);
                            }
                            let _ = fs::remove_file(&claim_path);
                            return ClaimedUnanswered(unanswered_dispatch_report(
                                "claim_owner_dead",
                                format!("daemon pid {claimer_pid} claimed {plugin}/{verb} task {task} and exited without answering -- the outcome is UNVERIFIED (a side-effecting verb may have applied some or all of its work), so it was NOT re-executed locally; read the real state (git log, the file, the store) before re-dispatching"),
                                plugin, verb, task.as_str(), Some(claimer_pid), waited_ms, &out_path,
                            ));
                        }
                    }
                }
                if at.elapsed() >= Duration::from_millis(claimed_timeout_ms) {
                    let claimer_pid =
                        find_plugin_dispatch_claim(&in_dir, &task).and_then(|(_, pid)| pid);
                    return ClaimedUnanswered(unanswered_dispatch_report(
                        "claimed_still_in_flight",
                        format!("the daemon claimed {plugin}/{verb} task {task} and has not answered within {claimed_timeout_ms}ms (AGENTPLUG_DISPATCH_CLAIMED_TIMEOUT_MS) -- it was NOT re-executed locally because the daemon may still be performing it; its answer will land at {}; read the real state before re-dispatching a side-effecting verb", out_path.display()),
                        plugin, verb, task.as_str(), claimer_pid, waited_ms, &out_path,
                    ));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(PLUGIN_DISPATCH_POLL_MS));
    }
}

