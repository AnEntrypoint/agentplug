use super::*;

pub(crate) type InFlightKey = (PathBuf, String, String);

pub(crate) struct InFlightHandle {
    pub(crate) detach: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) static IN_FLIGHT: OnceLock<Mutex<HashMap<InFlightKey, InFlightHandle>>> = OnceLock::new();

pub(crate) fn in_flight_map() -> &'static Mutex<HashMap<InFlightKey, InFlightHandle>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) const MAX_CLAIMED_DISPATCHES_PER_PROJECT: usize = 32;

pub(super) const LANE_WAIT_MAX_MS_DEFAULT: u64 = 120_000;
pub(super) const CODESEARCH_LANE_WAIT_MAX_MS: u64 = 5_000;
pub(super) const DISPATCH_WAIT_LEDGER_FILE: &str = ".dispatch-wait.json";
pub(super) const LEDGER_REFRESH_MIN_INTERVAL_MS: u64 = 250;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DispatchStage {
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

pub(super) fn note_dispatch_stage(
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

pub fn write_spool_out_confirmed(out_dir: &Path, out_name: &str, out_body: &str) -> bool {
    let dest = out_dir.join(out_name);
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

pub(super) fn write_spool_out_and_release_claim(
    out_dir: &Path,
    in_dir: &Path,
    verb: &str,
    task: &str,
    out_body: &str,
) {
    forget_in_flight_claim(in_dir, verb, task);
    if write_spool_out_confirmed(out_dir, &format!("{verb}-{task}.json"), out_body) {
        let _ = fs::remove_file(inflight_claim_path(in_dir, verb, task));
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

pub fn sweep_orphaned_claims(root: &Path) {
    sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &read_handoff_inherited_claims());
}

pub fn sweep_orphaned_claims_across_roots(roots: &[PathBuf]) {
    let inherited = read_handoff_inherited_claims();
    for root in roots {
        sweep_orphaned_claims_distinguishing_handoff_from_crash(root, &inherited);
    }
    clear_handoff_inherited_claims();
}

pub(super) const MIN_ORPHAN_CLAIM_AGE_MS: u64 = 60_000;

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
            let claim_age = claim_age_ms(&path);
            if claim_age
                .map(|age| age < MIN_ORPHAN_CLAIM_AGE_MS)
                .unwrap_or(false)
            {
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
                    "reason": "claim file with no live dispatch in this daemon (the claiming daemon exited or was handed off) -- reaped by the periodic orphan sweep so it cannot hold the project busy",
                    "claim_age_ms": claim_age,
                    "error": format!("verb {verb} (task {task}) was claimed by a daemon that stopped answering -- a wasm trap, an out-of-memory abort, or a shared-Store recycle during the call. A version handoff is NOT a cause of this error: a handoff re-queues its claims for the incoming daemon, which completes them. The outcome is UNVERIFIED, not known to be unperformed: a side-effecting verb (git_commit/git_finalize/git_push/fs_write/memorize-fire) may already have applied some or all of its work, so read the real state (git log, git status, the file, the store) before re-dispatching. Re-dispatch straight away only for a read-only verb."),
                    "verb": verb,
                    "task": task,
                    "sweeping_pid": std::process::id(),
                }).to_string();
                let confirmed = write_spool_out_confirmed(&out_dir, &out_name, &out_body);
                eprintln!("[agentplug daemon] swept orphaned claim {verb}/{task} for {} -- wrote error out-file", root.display());
                confirmed
            };
            if out_confirmed {
                let _ = fs::remove_file(&path);
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
    if !write_spool_out_confirmed(out_dir, &out_name, &out_body) {
        eprintln!("[agentplug daemon] could not write the spool_filename_rejected out-file for {verb}/{task}");
    }
}

pub(super) const SPOOL_OUT_REAP_INTERVAL_MS: u64 = 10 * 60 * 1000;
pub(super) const SPOOL_OUT_MIN_AGE_MS: u64 = 10 * 60 * 1000;
pub(super) const SPOOL_OUT_DEFAULT_MAX_FILES: usize = 3000;
pub(super) const SPOOL_OUT_DEFAULT_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
pub(super) const SPOOL_OUT_MAX_REAP_PER_PASS: usize = 1500;

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

pub(super) fn out_stamp_ms(stem: &str) -> Option<u64> {
    let mut tail = stem.rsplitn(3, '-');
    let seq = tail.next().unwrap_or_default();
    let stamp = tail.next().unwrap_or_default();
    if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if stamp.len() < 12 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stamp.parse::<u64>().ok()
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
    let mut stat_fallbacks = 0usize;
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
        let stamp_ms = match out_stamp_ms(&stem) {
            Some(ms) => ms,
            None => {
                stat_fallbacks += 1;
                entry
                    .metadata()
                    .ok()
                    .map(|m| out_meta_stamp_ms(&m, now))
                    .unwrap_or(now)
            }
        };
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
        let over_count_cap = index < over_cap;
        let expired_by_age = age_ms >= max_age_ms;
        if !(over_count_cap || expired_by_age) {
            continue;
        }
        remove_out_file_and_markers(&row.path);
        reaped += 1;
    }
    if reaped > 0 || stale_tmp > 0 || orphan_markers > 0 {
        eprintln!(
            "[agentplug daemon] reaped {} answered out-file(s), {} orphaned .ready marker(s) and {} stale tmp file(s) under {} -- out/ had {} entries ({} json, {} needed a stat), capped at {} files / {}h, {} dispatches still live",
            reaped,
            orphan_markers,
            stale_tmp,
            out_dir.display(),
            total_entries,
            rows.len(),
            stat_fallbacks,
            max_files,
            max_age_ms / (60 * 60 * 1000),
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
        "waited_ms": waited_ms,
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "error": format!(
            "verb {} (task {}) was claimed by daemon pid {} but never entered the {} admission gate within {}ms -- {} dispatches already hold the {} admitted slots of {}, so it was NOT executed and re-dispatching is safe. Live state for every unanswered request is in {}.",
            verb, task, std::process::id(), kind, waited_ms, in_flight, limit, slots,
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

pub(super) fn lane_wait_max(verb: &str) -> Duration {
    if verb == "codesearch" {
        return Duration::from_millis(CODESEARCH_LANE_WAIT_MAX_MS);
    }
    Duration::from_millis(env_ms_or(
        "AGENTPLUG_LANE_WAIT_MAX_MS",
        LANE_WAIT_MAX_MS_DEFAULT,
    ))
}

pub(super) fn lane_starved_answer(
    root: &Path,
    verb: &str,
    task: &str,
    lane: Option<&'static str>,
    report: &LaneWaitReport,
) -> String {
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
        "lane_holder_task": report.lane_holder_task,
        "daemon_pid": std::process::id(),
        "request_path": request_path.to_string_lossy(),
        "claim_path": claim_path.to_string_lossy(),
        "out_path": out_path.to_string_lossy(),
        "dispatch_wait_ledger": spool_dir.join(DISPATCH_WAIT_LEDGER_FILE).to_string_lossy(),
        "executed": false,
        "re_dispatch_safe": true,
        "error": format!(
            "verb {} (task {}) was claimed by daemon pid {} but never reached the {} serial lane within {}ms, so it was NOT executed -- re-dispatching is safe because nothing ran. The lane is held by one dispatch at a time per project (git, store, state lanes); holder task {}. Live state for every unanswered request is in {}.",
            verb, task, std::process::id(), lane.unwrap_or("unassigned"), report.waited_ms,
            report.lane_holder_task.as_deref().unwrap_or("unknown"),
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
            "verb {} (task {}) was claimed by daemon pid {} but never reached the front of the global {} FIFO within {}ms, so it was NOT executed -- re-dispatching is safe because nothing ran. That queue is global across every project this daemon serves, not per project; holder task {}. Live state for every unanswered request is in {}.",
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
    let lane = dispatch_serial_lane(tool_verb, body);
    let in_dir = root.join(".gm").join("exec-spool").join("in");
    note_dispatch_stage(root, verb, task, DispatchStage::WaitingForLane, lane);
    let _fairness_guard = match GmFairnessGuard::acquire_within(
        root,
        tool_verb,
        body,
        task,
        lane_wait_max(tool_verb),
    ) {
        Ok(guard) => guard,
        Err(report) => {
            let out_body = lane_starved_answer(root, verb, task, lane, &report);
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
    note_dispatch_stage(root, verb, task, DispatchStage::WaitingForToolQueue, lane);
    let _tool_guard = match ToolDispatchGuard::acquire_within(
        plugin_name,
        tool_verb,
        body,
        task,
        lane_wait_max(tool_verb),
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
    let out_body = match dispatch_result {
        Ok(Ok(s)) if !s.is_empty() => s,
        Ok(Ok(_)) => serde_json::json!({"ok": false, "error": "empty dispatch result", "verb": verb}).to_string(),
        Ok(Err(e)) => match e.downcast_ref::<agentplug_host::PluginDispatchError>() {
            Some(agentplug_host::PluginDispatchError::AdmissionStarved { kind, waited_ms, limit, in_flight, slots }) => {
                eprintln!(
                    "[agentplug daemon] {verb}/{task} for {} was claimed but never entered the {kind} admission gate within {waited_ms}ms -- answered dispatch_starved_waiting_for_admission; the verb did not run",
                    root.display()
                );
                admission_starved_answer(root, verb, task, lane, *kind, *waited_ms, *limit, *in_flight, *slots)
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
    let out_body = patch_update_available_from_escalation(plugin_name, verb, out_body);
    let out_body = spill_large_exec_output_to_text_sibling(out_dir, verb, task, out_body);
    let out_name = format!("{verb}-{task}.json");
    let out_confirmed = write_spool_out_confirmed(out_dir, &out_name, &out_body);
    if out_confirmed {
        let _ = fs::remove_file(inflight_claim_path(&in_dir, verb, task));
    } else {
        eprintln!("[agentplug daemon] out-file write for {verb}/{task} did not confirm for {} -- leaving the claim for the orphan sweep instead of deleting an unanswered request", root.display());
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

pub(super) fn refresh_dispatch_wait_ledger(root: &Path) {
    let spool_dir = spool_dir_of_root(root);
    let ledger_path = spool_dir.join(DISPATCH_WAIT_LEDGER_FILE);
    let (queued, claimed) = spool_step_counts(root);
    let records_empty = dispatch_wait_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    if records_empty && queued == 0 && claimed == 0 {
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
            let index_state = if CODE_INDEX_CONSUMING_VERBS.contains(&verb.as_str()) {
                Some(code_index_state(&spool_dir))
            } else {
                None
            };
            rows.push(serde_json::json!({
                "verb": verb,
                "task": task,
                "state": state,
                "lane": lane,
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
    let payload = serde_json::json!({
        "daemon_pid": std::process::id(),
        "ts": now,
        "root": root.to_string_lossy(),
        "project_in_flight": project_in_flight_count(root),
        "project_in_flight_cap": MAX_CLAIMED_DISPATCHES_PER_PROJECT,
        "requests": rows,
    })
    .to_string();
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
pub(super) struct IdleInDirWatch {
    entries: Vec<(PathBuf, PathBuf, windows_sys::Win32::Foundation::HANDLE)>,
}

#[cfg(windows)]
impl IdleInDirWatch {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
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
            .map(|root| (root.clone(), spool_dir_of(root).join("in")))
            .filter(|(_, dir)| dir.is_dir())
            .collect();
        if self
            .entries
            .iter()
            .map(|(_, dir, _)| dir)
            .eq(wanted.iter().map(|(_, dir)| dir))
        {
            return;
        }
        self.close_all();
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
    mark_spool_dirty(root);

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
    let in_dir_scan = fs::read_dir(&in_dir);
    let in_dir_existed = in_dir_scan.is_ok();
    let mut claimable: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    if let (true, Ok(entries)) = (project_has_queued_spool_work_cached(root), in_dir_scan) {
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

    if claimed.is_empty() && in_dir_existed && !project_has_pending_dispatch_work(root) {
        return did_work;
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
        } else {
            gm_requests.push(req);
        }
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

                let _tool_guard = ToolDispatchGuard::acquire(&plugin_name, &verb, &body);
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

