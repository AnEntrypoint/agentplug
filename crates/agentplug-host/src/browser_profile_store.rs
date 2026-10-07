use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::browser::{
    chrome_singleton_lock_present, cmdline_flag_value, last_used_sidecar_path, pid_is_alive,
    profile_dir_key, sanitize_pub, session_id_sidecar_path,
};

pub const PROFILE_DIR_PREFIX: &str = "browser-chrome-profile-";
pub const DEFAULT_SLOTS: usize = 4;
pub const DEFAULT_MAX_IDLE: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_COUNT: usize = 8;
pub const RESERVE_GRACE: Duration = Duration::from_secs(120);

pub struct Policy {
    pub slots: usize,
    pub max_idle: Duration,
    pub max_total_bytes: u64,
    pub max_count: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            slots: DEFAULT_SLOTS,
            max_idle: DEFAULT_MAX_IDLE,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_count: DEFAULT_MAX_COUNT,
        }
    }
}

pub struct Report {
    pub deleted: Vec<String>,
    pub freed_bytes: u64,
    pub kept: usize,
    pub kept_bytes: u64,
}

pub fn named_dir(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(".gm")
        .join(format!("{PROFILE_DIR_PREFIX}{}", sanitize_pub(session_id)))
}

pub fn slot_dir(cwd: &Path, slot: usize) -> PathBuf {
    cwd.join(".gm")
        .join(format!("{PROFILE_DIR_PREFIX}slot-{slot}"))
}

pub fn profile_dirs(cwd: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(cwd.join(".gm")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            (name.starts_with(PROFILE_DIR_PREFIX) && path.is_dir()).then_some(path)
        })
        .collect();
    dirs.sort();
    dirs
}

fn recorded_session_id(dir: &Path) -> Option<String> {
    std::fs::read_to_string(session_id_sidecar_path(dir))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|id| !id.is_empty())
}

pub fn resolve(cwd: &Path, session_id: &str) -> PathBuf {
    profile_dirs(cwd)
        .into_iter()
        .find(|dir| recorded_session_id(dir).as_deref() == Some(session_id))
        .unwrap_or_else(|| named_dir(cwd, session_id))
}

pub fn assign(
    cwd: &Path,
    session_id: &str,
    policy: &Policy,
    is_live: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(dir) = profile_dirs(cwd)
        .into_iter()
        .find(|dir| recorded_session_id(dir).as_deref() == Some(session_id))
    {
        return dir;
    }
    for slot in 0..policy.slots.max(1) {
        let dir = slot_dir(cwd, slot);
        if !is_live(&dir) {
            reserve(&dir, session_id);
            return dir;
        }
    }
    let fallback = named_dir(cwd, session_id);
    reserve(&fallback, session_id);
    fallback
}

fn reserve(dir: &Path, session_id: &str) {
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(session_id_sidecar_path(dir), session_id);
    }
}

pub fn profile_dir_is_live(
    dir: &Path,
    claimed: &HashSet<PathBuf>,
    processes: &[(u32, String)],
) -> bool {
    let key = profile_dir_key(&dir.to_string_lossy());
    if claimed
        .iter()
        .any(|claimed| profile_dir_key(&claimed.to_string_lossy()) == key)
    {
        return true;
    }
    if processes.iter().any(|(pid, cmdline)| {
        cmdline_flag_value(cmdline, "--user-data-dir=")
            .is_some_and(|used| profile_dir_key(&used) == key)
            && pid_is_alive(*pid)
    }) {
        return true;
    }
    if processes.is_empty() && chrome_singleton_lock_present(dir) {
        return true;
    }
    profile_dir_reserved_recently(dir)
}

fn profile_dir_reserved_recently(dir: &Path) -> bool {
    std::fs::metadata(session_id_sidecar_path(dir))
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < RESERVE_GRACE)
}

pub fn is_live_closure<'a>(
    claimed: &'a HashSet<PathBuf>,
    processes: &'a [(u32, String)],
) -> impl Fn(&Path) -> bool + 'a {
    move |dir| profile_dir_is_live(dir, claimed, processes)
}

pub fn reclaim(
    cwd: &Path,
    policy: &Policy,
    is_live: &dyn Fn(&Path) -> bool,
    measure_bytes: bool,
) -> Report {
    let mut candidates: Vec<Candidate> = profile_dirs(cwd)
        .into_iter()
        .map(|dir| Candidate {
            bytes: if measure_bytes { dir_bytes(&dir) } else { 0 },
            last_used: last_used(&dir),
            dir,
        })
        .collect();
    candidates.sort_by_key(|c| std::cmp::Reverse(c.last_used));

    let mut deleted: Vec<String> = Vec::new();
    let mut freed_bytes = 0u64;
    let mut protected_bytes = 0u64;
    let mut protected_count = 0usize;
    let mut pool: Vec<Candidate> = Vec::new();

    for candidate in candidates {
        if is_live(&candidate.dir) {
            protected_bytes += candidate.bytes;
            protected_count += 1;
            continue;
        }
        if idle_for(&candidate) > policy.max_idle {
            freed_bytes += remove(&candidate, measure_bytes, &mut deleted);
            continue;
        }
        pool.push(candidate);
    }

    let count_budget = policy.max_count.saturating_sub(protected_count);
    let byte_budget = policy.max_total_bytes.saturating_sub(protected_bytes);
    let mut kept_bytes = 0u64;
    let mut kept = Vec::new();
    for candidate in pool {
        let over_count = kept.len() + 1 > count_budget;
        let over_bytes = measure_bytes && kept_bytes + candidate.bytes > byte_budget;
        if (over_count || over_bytes) && !kept.is_empty() {
            freed_bytes += remove(&candidate, measure_bytes, &mut deleted);
            continue;
        }
        kept_bytes += candidate.bytes;
        kept.push(candidate);
    }

    Report {
        deleted,
        freed_bytes,
        kept: kept.len() + protected_count,
        kept_bytes: kept_bytes + protected_bytes,
    }
}

struct Candidate {
    dir: PathBuf,
    bytes: u64,
    last_used: SystemTime,
}

fn remove(candidate: &Candidate, measure_bytes: bool, deleted: &mut Vec<String>) -> u64 {
    let bytes = if measure_bytes {
        candidate.bytes
    } else {
        dir_bytes(&candidate.dir)
    };
    match std::fs::remove_dir_all(&candidate.dir) {
        Ok(()) => {
            deleted.push(dir_name(&candidate.dir));
            bytes
        }
        Err(_) => 0,
    }
}

fn idle_for(candidate: &Candidate) -> Duration {
    candidate
        .last_used
        .elapsed()
        .unwrap_or(Duration::from_secs(0))
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

pub fn last_used(dir: &Path) -> SystemTime {
    let sidecars: Vec<SystemTime> = [session_id_sidecar_path(dir), last_used_sidecar_path(dir)]
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
        .collect();
    if let Some(newest) = sidecars.into_iter().max() {
        return newest;
    }
    std::fs::metadata(dir)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}
