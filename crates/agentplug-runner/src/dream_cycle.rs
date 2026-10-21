use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERB: &str = "dream-replay-cycle";
const COOLDOWN_MS: u64 = 15 * 60 * 1000;
const MAX_BACKOFF_MS: u64 = 60 * 60 * 1000;
const MIN_NEW_OBSERVATIONS: usize = 20;
const BACKSTOP_IDLE_MS: u64 = 10 * 60 * 1000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_OWNERS_PER_PASS: usize = 64;
const WALK_BUDGET: Duration = Duration::from_millis(50);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OwnerState {
    schema: u32,
    owner_session_id: String,
    last_dispatch_id: Option<String>,
    next_attempt_ms: u64,
    pending: Option<String>,
    last_status: Option<String>,
    #[serde(default)]
    refusals: u32,
    #[serde(default)]
    permanent_observations_mtime_ms: Option<u64>,
    #[serde(default)]
    request_writes: u32,
}

#[derive(Default)]
pub struct CycleTicker {
    next_root: usize,
    cursors: HashMap<PathBuf, String>,
    pending_cursors: HashMap<PathBuf, String>,
    observed: HashMap<(PathBuf, String), u64>,
}

fn list_owners(root: &Path) -> io::Result<Vec<String>> {
    let mut owners = Vec::new();
    for entry in fs::read_dir(root.join(".gm/dream-rsi"))? {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            owners.push(name.to_owned());
        }
    }
    owners.sort_unstable();
    Ok(owners)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !matches!(id, "." | "..")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn read_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("cycle input is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::other("cycle file exceeds read bound"));
    }
    Ok(bytes)
}

/// Reads the live observations file. The writer rotates it by record count (256 JSONL lines),
/// so it can outgrow MAX_FILE_BYTES; an oversized file keeps its newest MAX_FILE_BYTES bytes and
/// drops the record cut at the window start, so the cycle runs instead of erroring every tick.
fn read_observations(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("cycle input is not a regular file"));
    }
    let len = meta.len();
    let windowed = len > MAX_FILE_BYTES;
    if windowed {
        file.seek(SeekFrom::Start(len - MAX_FILE_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes)?;
    if windowed {
        match bytes.iter().position(|&byte| byte == b'\n') {
            Some(end) => {
                bytes.drain(..=end);
            }
            None => bytes.clear(),
        }
    }
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut owns_temporary = false;
    let result = (|| {
        let mut file = options.open(&temporary)?;
        owns_temporary = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("cycle path lacks parent"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn persist(path: &Path, state: &OwnerState) -> io::Result<()> {
    let bytes = serde_json::to_vec(state)?;
    atomic_write(path, &bytes)
}

fn request_body(owner: &str, pending: &str, state: &OwnerState) -> Value {
    let mut body = json!({
        "session_id": owner, "cycle_id": pending,
        "trigger": "unattended-daemon-observation-cycle"
    });
    if let Some(after) = &state.last_dispatch_id {
        body["after_dispatch_id"] = json!(after);
    }
    if state.last_status.as_deref() == Some("pending-partial-window") {
        body["partial_window"] = json!(true);
    }
    body
}

const MAX_SPOOL_TASK_ID_BYTES: usize = 200;
const PERMANENT_REFUSALS: u32 = 3;
const MAX_REQUEST_WRITES: u32 = 3;

fn observations_mtime_ms(owner_dir: &Path) -> Option<u64> {
    fs::metadata(owner_dir.join("observations.json"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as u64)
}

fn owner_has_pending(owner_dir: &Path) -> bool {
    read_bounded(&owner_dir.join(".cycle-state.json"), 4096)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|state| state.get("pending").is_some_and(|pending| !pending.is_null()))
}

fn spool_task_id(owner: &str, cycle_id: &str) -> io::Result<String> {
    let task = format!("{owner}-{cycle_id}");
    if task.len() > MAX_SPOOL_TASK_ID_BYTES {
        return Err(io::Error::other(
            "cycle task id exceeds the spool file-name bound",
        ));
    }
    Ok(task)
}

fn backoff_ms(refusals: u32) -> u64 {
    let doublings = refusals.saturating_sub(1).min(16);
    COOLDOWN_MS
        .saturating_mul(1 << doublings)
        .min(MAX_BACKOFF_MS)
}

fn settle(
    path: &Path,
    state: &mut OwnerState,
    now: u64,
    status: &str,
    replayed: bool,
) -> io::Result<()> {
    let legitimate = matches!(status, "deferred" | "deferred-no-new-verified-dispatch");
    state.refusals = if replayed {
        0
    } else if legitimate {
        state.refusals
    } else {
        state.refusals.saturating_add(1)
    };
    let wait = if legitimate {
        COOLDOWN_MS
    } else {
        backoff_ms(state.refusals)
    };
    state.next_attempt_ms = now.saturating_add(wait);
    state.last_status = Some(status.into());
    if !replayed {
        eprintln!(
            "[dream cycle] {} {status}: refusal {} backs off {} s",
            path.display(),
            state.refusals,
            wait / 1000
        );
    }
    persist(path, state)
}

fn defer_reply(
    path: &Path,
    state: &mut OwnerState,
    now: u64,
    reason: &str,
    pending_active: bool,
) -> io::Result<()> {
    if !pending_active {
        state.pending = None;
    }
    settle(path, state, now, reason, false)
}

fn note_skip(path: &Path, state: &mut OwnerState, status: &str) -> io::Result<()> {
    if state.last_status.as_deref() == Some(status) {
        return Ok(());
    }
    eprintln!("[dream cycle] {} {status}", path.display());
    state.last_status = Some(status.into());
    persist(path, state)
}

fn cycle_result_current(owner_dir: &Path, rows: &[Value]) -> bool {
    let Some(newest_observation) = rows
        .iter()
        .filter_map(|row| row.get("ts").and_then(Value::as_i64))
        .max()
    else {
        return false;
    };
    let Ok(bytes) = read_bounded(&owner_dir.join("cycle-result.json"), MAX_FILE_BYTES) else {
        return false;
    };
    let Ok(result) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    result
        .get("ts")
        .and_then(Value::as_i64)
        .is_some_and(|ts| ts >= newest_observation)
}

#[derive(Deserialize)]
struct Replay {
    replays: Option<Vec<ReplayRow>>,
}

#[derive(Deserialize)]
struct ReplayRow {
    dispatch_id: Option<String>,
}

#[derive(Deserialize)]
struct CycleBody {
    kind: Option<String>,
    owner_session_id: Option<String>,
    cycle_id: Option<String>,
    status: Option<String>,
    last_dispatch_id: Option<String>,
    replay: Option<Replay>,
}

#[derive(Deserialize)]
struct CycleReply {
    ok: Option<bool>,
    data: Option<Box<CycleReply>>,
    stdout: Option<String>,
    #[serde(flatten)]
    body: CycleBody,
}

fn select_body(reply: CycleReply) -> Option<CycleReply> {
    if reply.body.kind.as_deref() == Some("observations") {
        return Some(reply);
    }
    if let Some(data) = reply.data {
        if data.body.kind.as_deref() == Some("observations") {
            return Some(*data);
        }
        if let Some(stdout) = data.stdout.as_deref() {
            return serde_json::from_str(stdout).ok();
        }
    }
    serde_json::from_str(reply.stdout.as_deref()?).ok()
}

fn check_reply(bytes: &[u8], owner: &str, cycle_id: &str) -> Result<CycleReply, &'static str> {
    let reply: CycleReply =
        serde_json::from_slice(bytes).map_err(|_| "deferred-malformed-reply")?;
    if reply.ok != Some(true) {
        return Err("reject-not-ok");
    }
    let body = select_body(reply).ok_or("reject-no-observations-body")?;
    if body.ok != Some(true) {
        return Err("reject-not-ok");
    }
    if body.body.kind.as_deref() != Some("observations") {
        return Err("reject-wrong-kind");
    }
    if body.body.owner_session_id.as_deref() != Some(owner) {
        return Err("reject-wrong-owner");
    }
    if body.body.cycle_id.as_deref() != Some(cycle_id) {
        return Err("reject-wrong-cycle");
    }
    Ok(body)
}

fn parse_observation_rows(bytes: &[u8]) -> io::Result<Vec<Value>> {
    if let Ok(Value::Array(rows)) = serde_json::from_slice::<Value>(bytes) {
        return Ok(rows);
    }
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).map_err(io::Error::other))
        .collect()
}

fn row_dispatch_id(row: &Value) -> Option<&str> {
    row.get("dispatch_id").and_then(Value::as_str)
}

fn archived_rows(owner_dir: &Path) -> io::Result<Vec<Value>> {
    let mut archives: Vec<(i64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(owner_dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(ts) = name
            .to_str()
            .and_then(|name| name.strip_prefix("observations-archive-"))
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|ts| ts.parse::<i64>().ok())
        else {
            continue;
        };
        archives.push((ts, entry.path()));
    }
    archives.sort_unstable_by_key(|(ts, _)| *ts);
    let mut rows = Vec::new();
    for (_, path) in archives {
        let bytes = read_bounded(&path, MAX_FILE_BYTES)?;
        rows.extend(parse_observation_rows(&bytes)?);
    }
    Ok(rows)
}

pub fn process_owner(root: &Path, owner: &str) -> io::Result<()> {
    loop {
        if !safe_id(owner) {
            return Err(io::Error::other("unsafe cycle owner"));
        }
        let owner_dir = root.join(".gm/dream-rsi").join(owner);
        let state_path = owner_dir.join(".cycle-state.json");
        let mut state = match read_bounded(&state_path, 4096) {
            Ok(bytes) => {
                let state: OwnerState = serde_json::from_slice(&bytes)?;
                if state.schema != 1
                    || state.owner_session_id != owner
                    || state.pending.as_deref().is_some_and(|id| !safe_id(id))
                    || state
                        .last_dispatch_id
                        .as_ref()
                        .is_some_and(|id| id.is_empty() || id.len() > 512)
                {
                    return Err(io::Error::other("invalid cycle owner state"));
                }
                state
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => OwnerState {
                schema: 1,
                owner_session_id: owner.into(),
                ..OwnerState::default()
            },
            Err(error) => return Err(error),
        };
        if state.last_status.as_deref() == Some("reply-refused-or-invalid") {
            state.last_status = None;
            state.next_attempt_ms = 0;
            persist(&state_path, &state)?;
        }
        let observations_mtime = observations_mtime_ms(&owner_dir);
        if let Some(recorded) = state.permanent_observations_mtime_ms.filter(|_| state.pending.is_none()) {
            if observations_mtime == Some(recorded) {
                return Ok(());
            }
            state.permanent_observations_mtime_ms = None;
            state.refusals = 0;
        }
        let spool = root.join(".gm/exec-spool");
        let input_dir = spool.join("in").join(VERB);
        let now = now_ms();
        if now < state.next_attempt_ms {
            return Ok(());
        }
        if let Some(pending) = state.pending.clone() {
            let task = spool_task_id(owner, &pending)?;
            let input = input_dir.join(format!("{task}.txt"));
            let claim = input_dir.join(format!("{task}.txt.inflight"));
            let pending_active = input.try_exists()? || claim.try_exists()?;
            let output_path = spool.join("out").join(format!("{VERB}-{task}.json"));
            match read_bounded(&output_path, MAX_FILE_BYTES) {
                Ok(bytes) => {
                    let checked = check_reply(&bytes, owner, &pending);
                    if pending_active {
                        return match checked {
                            Ok(_) => {
                                state.last_status = Some("awaiting-terminal-claim-release".into());
                                persist(&state_path, &state)
                            }
                            Err(reason) => defer_reply(&state_path, &mut state, now, reason, true),
                        };
                    }
                    let mut replayed = false;
                    let status = match checked {
                        Err(reason) => reason,
                        Ok(reply) => match reply.body.status.as_deref() {
                            Some("replayed") => {
                                let latest = reply.body.last_dispatch_id.as_deref();
                                let verified = reply
                                    .body
                                    .replay
                                    .as_ref()
                                    .and_then(|replay| replay.replays.as_ref())
                                    .and_then(|rows| rows.first())
                                    .and_then(|row| row.dispatch_id.as_deref())
                                    .or(latest);
                                if latest.is_some()
                                    && latest == verified
                                    && latest.is_some_and(|id| !id.is_empty() && id.len() <= 512)
                                    && latest != state.last_dispatch_id.as_deref()
                                {
                                    state.last_dispatch_id = latest.map(str::to_owned);
                                    replayed = true;
                                    "replayed"
                                } else {
                                    "deferred-no-new-verified-dispatch"
                                }
                            }
                            Some("deferred") => "deferred",
                            _ => "reject-unknown-status",
                        },
                    };
                    state.pending = None;
                    let refused_for_good = status.starts_with("reject-")
                        && state.refusals.saturating_add(1) >= PERMANENT_REFUSALS;
                    let deferred_until_new_observations =
                        matches!(status, "deferred" | "deferred-no-new-verified-dispatch");
                    if refused_for_good || deferred_until_new_observations {
                        state.permanent_observations_mtime_ms = observations_mtime;
                    }
                    return settle(&state_path, &mut state, now, status, replayed);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if pending_active {
                        return Ok(());
                    }
                    if state.request_writes >= MAX_REQUEST_WRITES {
                        state.pending = None;
                        state.permanent_observations_mtime_ms = observations_mtime;
                        return settle(
                            &state_path,
                            &mut state,
                            now,
                            "deferred-no-reply-after-requests",
                            false,
                        );
                    }
                    state.request_writes += 1;
                    persist(&state_path, &state)?;
                    fs::create_dir_all(&input_dir)?;
                    return atomic_write(
                        &input,
                        request_body(owner, &pending, &state).to_string().as_bytes(),
                    );
                }
                Err(_) if pending_active => return Ok(()),
                Err(_) => {
                    return defer_reply(
                        &state_path,
                        &mut state,
                        now,
                        "deferred-unreadable-or-oversized-reply",
                        pending_active,
                    )
                }
            }
        }
        let observations_bytes =
            match read_observations(&owner_dir.join("observations.json")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
        let parsed_rows = parse_observation_rows(&observations_bytes)?;
        let rows = &parsed_rows[parsed_rows.len().saturating_sub(256)..];
        let live_cursor = state.last_dispatch_id.as_deref().and_then(|id| {
            parsed_rows
                .iter()
                .rposition(|row| row_dispatch_id(row) == Some(id))
        });
        let (new_rows, cursor_found): (Vec<Value>, bool) = match (state.last_dispatch_id.as_deref(), live_cursor) {
            (_, Some(index)) => (parsed_rows[index + 1..].to_vec(), true),
            (Some(id), None) => {
                let history = archived_rows(&owner_dir)?;
                match history.iter().rposition(|row| row_dispatch_id(row) == Some(id)) {
                    Some(index) => {
                        let gap = &history[index + 1..];
                        let seen: HashSet<&str> = gap.iter().filter_map(row_dispatch_id).collect();
                        let mut merged = gap.to_vec();
                        merged.extend(
                            rows.iter()
                                .filter(|row| row_dispatch_id(row).map_or(true, |rid| !seen.contains(rid)))
                                .cloned(),
                        );
                        (merged, true)
                    }
                    None => (rows.to_vec(), false),
                }
            }
            (None, None) => (rows.to_vec(), false),
        };
        let ended = new_rows
            .iter()
            .any(|row| row.get("run_end").and_then(Value::as_bool) == Some(true));
        let idle = rows
            .last()
            .and_then(|row| row.get("ts").and_then(Value::as_i64))
            .is_some_and(|ts| (now as i64).saturating_sub(ts) >= BACKSTOP_IDLE_MS as i64);
        let backstop = new_rows.len() >= MIN_NEW_OBSERVATIONS && idle;
        if !(ended || backstop) {
            return Ok(());
        }
        if cycle_result_current(&owner_dir, &rows) {
            return note_skip(
                &state_path,
                &mut state,
                "skipped-cycle-result-newer-than-observations",
            );
        }
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        state.pending = Some(format!(
            "dream-cycle-{}-{now}-{sequence}",
            std::process::id()
        ));
        state.request_writes = 0;
        state.last_status = Some(
            if state.last_dispatch_id.is_some() && !cursor_found {
                "pending-partial-window"
            } else {
                "pending"
            }
            .into(),
        );
        persist(&state_path, &state)?;
    }
}

impl CycleTicker {
    pub fn tick(&mut self, roots: &[PathBuf]) {
        self.cursors.retain(|root, _| roots.contains(root));
        self.pending_cursors.retain(|root, _| roots.contains(root));
        self.observed.retain(|(root, _), _| roots.contains(root));
        if roots.is_empty() {
            return;
        }
        let mut roots_visited = 0;
        while roots_visited < roots.len() {
            let root = roots[self.next_root % roots.len()].clone();
            self.next_root = (self.next_root + 1) % roots.len();
            roots_visited += 1;
            let owners = match list_owners(&root) {
                Ok(owners) => owners,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    eprintln!("[dream cycle] owner walk deferred: {error}");
                    continue;
                }
            };
            let pending_owners: Vec<&String> = owners
                .iter()
                .filter(|owner| owner_has_pending(&root.join(".gm/dream-rsi").join(owner)))
                .collect();
            let pending_start = self.pending_cursors.get(&root).map_or(0, |cursor| {
                pending_owners.partition_point(|owner| owner.as_str() <= cursor.as_str())
            });
            let mut pending_last = None;
            for offset in 0..pending_owners.len().min(MAX_OWNERS_PER_PASS) {
                let owner = pending_owners[(pending_start + offset) % pending_owners.len()];
                if let Err(error) = process_owner(&root, owner) {
                    eprintln!(
                        "[dream cycle] {} owner {owner} deferred: {error}",
                        root.display()
                    );
                }
                pending_last = Some(owner.clone());
            }
            match pending_last {
                Some(last) => {
                    self.pending_cursors.insert(root.clone(), last);
                }
                None => {
                    self.pending_cursors.remove(&root);
                }
            }
            let mut changed: Vec<(u64, &String)> = owners
                .iter()
                .filter_map(|owner| {
                    let owner_dir = root.join(".gm/dream-rsi").join(owner);
                    let mtime = observations_mtime_ms(&owner_dir).unwrap_or(0);
                    (self.observed.get(&(root.clone(), owner.clone())) != Some(&mtime))
                        .then_some((mtime, owner))
                })
                .collect();
            changed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            if let Some(oldest) = changed.pop() {
                changed.insert(0, oldest);
            }
            let changed_start = Instant::now();
            let mut changed_visited = 0;
            for (mtime, owner) in changed {
                if changed_visited >= MAX_OWNERS_PER_PASS
                    || changed_start.elapsed() >= WALK_BUDGET
                {
                    break;
                }
                changed_visited += 1;
                self.observed.insert((root.clone(), owner.clone()), mtime);
                if let Err(error) = process_owner(&root, owner) {
                    eprintln!(
                        "[dream cycle] {} owner {owner} deferred: {error}",
                        root.display()
                    );
                }
            }
            let resume = self.cursors.get(&root).map_or(0, |cursor| {
                owners.partition_point(|owner| owner.as_str() <= cursor.as_str())
            });
            let resume_start = Instant::now();
            let mut resumed = 0;
            let mut last_visited = None;
            let mut exhausted = true;
            for owner in &owners[resume..] {
                if resumed >= MAX_OWNERS_PER_PASS || resume_start.elapsed() >= WALK_BUDGET {
                    exhausted = false;
                    break;
                }
                resumed += 1;
                if let Err(error) = process_owner(&root, owner) {
                    eprintln!(
                        "[dream cycle] {} owner {owner} deferred: {error}",
                        root.display()
                    );
                }
                last_visited = Some(owner.clone());
            }
            match (exhausted, last_visited) {
                (true, _) => {
                    self.cursors.remove(&root);
                }
                (false, Some(last)) => {
                    self.cursors.insert(root, last);
                }
                (false, None) => {}
            }
        }
    }
}

pub fn spawn(
    interval: Duration,
    roots: fn() -> Vec<PathBuf>,
    authority_lost: fn() -> bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut ticker = CycleTicker::default();
        loop {
            std::thread::sleep(interval);
            if authority_lost() {
                return;
            }
            ticker.tick(&roots());
        }
    })
}
