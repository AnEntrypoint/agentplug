use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions, ReadDir};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERB: &str = "dream-replay-cycle";
const COOLDOWN_MS: u64 = 15 * 60 * 1000;
const MIN_NEW_OBSERVATIONS: usize = 20;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_OWNERS_PER_PASS: usize = 64;
const MAX_ENTRIES_PER_ROOT_BATCH: usize = 8;
const MAX_ROOT_SCANNERS: usize = 256;
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
}

#[derive(Default)]
pub struct CycleTicker {
    next_root: usize,
    scans: HashMap<PathBuf, ReadDir>,
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
    body
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
    state.next_attempt_ms = now.saturating_add(COOLDOWN_MS);
    state.last_status = Some(reason.into());
    persist(path, state)
}

fn reply_value(reply: &Value) -> Option<Value> {
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if reply.get("kind").and_then(Value::as_str) == Some("observations") {
        return Some(reply.clone());
    }
    if let Some(data) = reply.get("data") {
        if data.get("kind").and_then(Value::as_str) == Some("observations") {
            return Some(data.clone());
        }
        if let Some(stdout) = data.get("stdout").and_then(Value::as_str) {
            return serde_json::from_str(stdout).ok();
        }
    }
    reply
        .get("stdout")
        .and_then(Value::as_str)
        .and_then(|stdout| serde_json::from_str(stdout).ok())
}

pub fn process_owner(root: &Path, owner: &str) -> io::Result<()> {
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
    let spool = root.join(".gm/exec-spool");
    let input_dir = spool.join("in").join(VERB);
    let now = now_ms();
    if now < state.next_attempt_ms {
        return Ok(());
    }
    if let Some(pending) = state.pending.clone() {
        let input = input_dir.join(format!("{pending}.txt"));
        let claim = input_dir.join(format!("{pending}.txt.inflight"));
        let pending_active = input.try_exists()? || claim.try_exists()?;
        let output_path = spool.join("out").join(format!("{VERB}-{pending}.json"));
        match read_bounded(&output_path, MAX_FILE_BYTES) {
            Ok(bytes) => {
                let reply: Value = match serde_json::from_slice(&bytes) {
                    Ok(reply) => reply,
                    Err(_) => {
                        return defer_reply(
                            &state_path,
                            &mut state,
                            now,
                            "deferred-malformed-reply",
                            pending_active,
                        )
                    }
                };
                let value = reply_value(&reply);
                let valid = value.as_ref().filter(|value| {
                    value.get("ok").and_then(Value::as_bool) == Some(true)
                        && value.get("kind").and_then(Value::as_str) == Some("observations")
                        && value.get("owner_session_id").and_then(Value::as_str) == Some(owner)
                        && value.get("cycle_id").and_then(Value::as_str) == Some(pending.as_str())
                });
                if pending_active {
                    if valid.is_some() {
                        state.last_status = Some("awaiting-terminal-claim-release".into());
                        return persist(&state_path, &state);
                    }
                    return defer_reply(
                        &state_path,
                        &mut state,
                        now,
                        "deferred-nonterminal-invalid-reply",
                        true,
                    );
                }
                state.last_status = Some("reply-refused-or-invalid".into());
                if let Some(value) = valid {
                    match value.get("status").and_then(Value::as_str) {
                        Some("replayed") => {
                            let latest = value.get("last_dispatch_id").and_then(Value::as_str);
                            let verified = value
                                .get("replay")
                                .and_then(|v| v.get("replays"))
                                .and_then(Value::as_array)
                                .and_then(|rows| rows.first())
                                .and_then(|row| row.get("dispatch_id"))
                                .and_then(Value::as_str);
                            if latest.is_some()
                                && latest == verified
                                && latest.is_some_and(|id| !id.is_empty() && id.len() <= 512)
                                && latest != state.last_dispatch_id.as_deref()
                            {
                                state.last_dispatch_id = latest.map(str::to_owned);
                                state.last_status = Some("replayed".into());
                            } else {
                                state.last_status =
                                    Some("deferred-no-new-verified-dispatch".into());
                            }
                        }
                        Some("deferred") => state.last_status = Some("deferred".into()),
                        _ => {}
                    }
                }
                state.pending = None;
                state.next_attempt_ms = now.saturating_add(COOLDOWN_MS);
                return persist(&state_path, &state);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if pending_active {
                    return Ok(());
                }
                fs::create_dir_all(&input_dir)?;
                return atomic_write(
                    &input,
                    request_body(owner, &pending, &state).to_string().as_bytes(),
                );
            }
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
    let observations_bytes = match read_bounded(&owner_dir.join("observations.json"), MAX_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let observations: Value = serde_json::from_slice(&observations_bytes)?;
    let rows = observations
        .as_array()
        .ok_or_else(|| io::Error::other("cycle observations are not an array"))?;
    if rows.len() > 256 {
        return Err(io::Error::other("cycle observations exceed owner window"));
    }
    let cursor = state.last_dispatch_id.as_deref().and_then(|id| {
        rows.iter()
            .rposition(|row| row.get("dispatch_id").and_then(Value::as_str) == Some(id))
    });
    let enough = match (state.last_dispatch_id.as_ref(), cursor) {
        (_, Some(index)) => rows.len().saturating_sub(index + 1) >= MIN_NEW_OBSERVATIONS,
        (None, _) => rows.len() >= MIN_NEW_OBSERVATIONS,
        (Some(_), None) => !rows.is_empty(),
    };
    if !enough {
        return Ok(());
    }
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    state.pending = Some(format!(
        "dream-cycle-{}-{now}-{sequence}",
        std::process::id()
    ));
    state.last_status = Some(
        if state.last_dispatch_id.is_some() && cursor.is_none() {
            "pending-partial-window"
        } else {
            "pending"
        }
        .into(),
    );
    persist(&state_path, &state)?;
    process_owner(root, owner)
}

impl CycleTicker {
    pub fn tick(&mut self, roots: &[PathBuf]) {
        self.scans.retain(|root, _| roots.contains(root));
        if roots.is_empty() {
            return;
        }
        let start = Instant::now();
        let mut visited = 0;
        let mut roots_visited = 0;
        while visited < MAX_OWNERS_PER_PASS
            && roots_visited < roots.len()
            && start.elapsed() < WALK_BUDGET
        {
            let root = roots[self.next_root % roots.len()].clone();
            self.next_root = (self.next_root + 1) % roots.len();
            roots_visited += 1;
            if !self.scans.contains_key(&root) {
                if self.scans.len() >= MAX_ROOT_SCANNERS {
                    eprintln!("[dream cycle] owner walk deferred: root cursor cache full");
                    continue;
                }
                match fs::read_dir(root.join(".gm/dream-rsi")) {
                    Ok(entries) => {
                        self.scans.insert(root.clone(), entries);
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        eprintln!("[dream cycle] owner walk deferred: {error}");
                        continue;
                    }
                }
            }
            let Some(entries) = self.scans.get_mut(&root) else {
                continue;
            };
            let mut exhausted = false;
            for _ in 0..MAX_ENTRIES_PER_ROOT_BATCH {
                if visited >= MAX_OWNERS_PER_PASS || start.elapsed() >= WALK_BUDGET {
                    break;
                }
                match entries.next() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(Err(error)) => {
                        visited += 1;
                        eprintln!("[dream cycle] owner entry deferred: {error}");
                    }
                    Some(Ok(entry)) => {
                        visited += 1;
                        match entry.file_type() {
                            Ok(kind) if kind.is_dir() => {
                                if let Some(owner) = entry.file_name().to_str() {
                                    if let Err(error) = process_owner(&root, owner) {
                                        eprintln!(
                                            "[dream cycle] {} owner {owner} deferred: {error}",
                                            root.display()
                                        );
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => eprintln!("[dream cycle] owner type deferred: {error}"),
                        }
                    }
                }
            }
            if exhausted {
                self.scans.remove(&root);
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
