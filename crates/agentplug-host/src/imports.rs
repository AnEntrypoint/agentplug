use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;
use wasmtime::{AsContextMut, Caller, Linker, Memory};

use crate::host_state::HostState;

const HOST_FS_READ_EMPTY_SUCCESS: u64 = 1;

const HOST_BINARY_READ_MAX_BYTES: u64 = 20 * 1024 * 1024;

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let chunk_bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let packed_triple = (u32::from(chunk_bytes[0]) << 16) | (u32::from(chunk_bytes[1]) << 8) | u32::from(chunk_bytes[2]);
        out.push(ALPHABET[(packed_triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(packed_triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(packed_triple >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[packed_triple as usize & 63] as char } else { '=' });
    }
    out
}

pub(crate) fn fetch_agent(timeout: Duration) -> ureq::Agent {
    crate::http_agent::build_agent(timeout)
}

const GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE: u64 = 300_000;

const GIT_LAYOUT_QUESTION_FLAGS: &[&str] = &[
    "--show-toplevel",
    "--show-prefix",
    "--show-cdup",
    "--git-dir",
    "--absolute-git-dir",
    "--is-inside-work-tree",
    "--is-inside-git-dir",
];

const GIT_LAYOUT_CACHE_MAX_ENTRIES: usize = 512;

const GIT_INDEX_ANSWER_MAX_BYTES: usize = 1 << 20;

#[derive(Clone)]
pub(crate) struct GitLayoutAnswer {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) fingerprint: Option<String>,
}

pub(crate) fn git_layout_cache() -> &'static Mutex<HashMap<String, (String, GitLayoutAnswer)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, GitLayoutAnswer)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn git_layout_dir_key(dir: &Path) -> String {
    dir.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn git_layout_dirs_overlap(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn git_argv_fold<'a>(cwd: &'a Path, argv: &'a [String]) -> Option<(PathBuf, Vec<&'a str>)> {
    let mut dir = cwd.to_path_buf();
    let mut rest: Vec<&str> = Vec::with_capacity(argv.len());
    let mut i = 0usize;
    while i < argv.len() {
        match argv[i].as_str() {
            "-c" => i += 2,
            "-C" => {
                let value = argv.get(i + 1)?;
                let given = PathBuf::from(value);
                dir = if given.is_absolute() { given } else { dir.join(given) };
                i += 2;
            }
            other => {
                rest.push(other);
                i += 1;
            }
        }
    }
    Some((dir, rest))
}

pub(crate) fn git_layout_question(cwd: &Path, argv: &[String]) -> Option<(PathBuf, String)> {
    let (dir, rest) = git_argv_fold(cwd, argv)?;
    if rest.len() != 2 || rest[0] != "rev-parse" || !GIT_LAYOUT_QUESTION_FLAGS.contains(&rest[1]) {
        return None;
    }
    Some((dir, format!("rev-parse {}", rest[1])))
}

fn git_index_question(cwd: &Path, argv: &[String]) -> Option<(PathBuf, String)> {
    let (dir, rest) = git_argv_fold(cwd, argv)?;
    if rest.len() != 4 && !(rest.len() == 6 && rest[4] == "--") {
        return None;
    }
    if rest[0] != "--literal-pathspecs" || rest[1] != "ls-files" || rest[2] != "-z" || rest[3] != "--stage" {
        return None;
    }
    Some((dir, rest.join(" ")))
}

fn git_head_question(cwd: &Path, argv: &[String]) -> Option<PathBuf> {
    let (dir, rest) = git_argv_fold(cwd, argv)?;
    match rest.as_slice() {
        ["rev-parse", "HEAD"] | ["rev-parse", "--verify", "HEAD"] => Some(dir),
        _ => None,
    }
}

pub(crate) fn git_dir_of(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if !dot_git.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let rest = text.trim().strip_prefix("gitdir:")?.trim();
    let given = PathBuf::from(rest);
    Some(if given.is_absolute() {
        given
    } else {
        dir.join(given)
    })
}

fn git_file_identity(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())?;
    Some(format!("{mtime_ms}:{}", meta.len()))
}

fn git_index_identity(dir: &Path) -> Option<String> {
    git_file_identity(&git_dir_of(dir)?.join("index"))
}

fn git_head_ref_contents(git_dir: &Path, reference: &str) -> Option<String> {
    let direct = git_dir.join(reference);
    if direct.is_file() {
        return fs::read_to_string(&direct).ok();
    }
    let common = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        git_dir.join(common)
    };
    fs::read_to_string(common.join(reference)).ok()
}

fn git_head_sha_from_disk(dir: &Path) -> Option<String> {
    let git_dir = git_dir_of(dir)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let sha = match head.strip_prefix("ref:") {
        Some(reference) => git_head_ref_contents(&git_dir, reference.trim())?,
        None => head.to_string(),
    };
    let sha = sha.trim();
    if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
    }
}

pub(crate) fn git_layout_recall(dir: &Path, question: &str, live: Option<String>) -> Option<GitLayoutAnswer> {
    let key = format!("{}\u{0}{}", git_layout_dir_key(dir), question);
    let mut cache = match git_layout_cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    let answer = cache.get(&key).map(|(_, answer)| answer.clone());
    let Some(answer) = answer else {
        return None;
    };
    match (answer.fingerprint.as_ref(), live.as_ref()) {
        (Some(kept), Some(now)) if kept != now => {
            cache.remove(&key);
            None
        }
        (Some(_), None) => {
            cache.remove(&key);
            None
        }
        _ => Some(answer),
    }
}

pub(crate) fn git_layout_remember(dir: PathBuf, question: &str, answer: GitLayoutAnswer) {
    let dir_key = git_layout_dir_key(&dir);
    let key = format!("{dir_key}\u{0}{question}");
    let mut cache = match git_layout_cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    if cache.len() >= GIT_LAYOUT_CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, (dir_key, answer));
}

pub(crate) fn git_layout_forget_dir(dir: &Path) {
    let key = git_layout_dir_key(dir);
    let mut cache = match git_layout_cache().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache.retain(|_, (dir_key, answer)| {
        answer.fingerprint.is_some() || !git_layout_dirs_overlap(dir_key, &key)
    });
}

const GIT_CALL_STATS_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

const GIT_CALL_STATS_MAX_KEYS: usize = 256;

fn git_call_stats_counts(reached: bool) -> &'static Mutex<HashMap<String, usize>> {
    static ASKED: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    static REACHED: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    if reached {
        REACHED.get_or_init(|| Mutex::new(HashMap::new()))
    } else {
        ASKED.get_or_init(|| Mutex::new(HashMap::new()))
    }
}

fn git_call_stats_ms() -> &'static Mutex<HashMap<String, u64>> {
    static MS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    MS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn git_call_stats_last_flush() -> &'static Mutex<Instant> {
    static LAST_FLUSH: OnceLock<Mutex<Instant>> = OnceLock::new();
    LAST_FLUSH.get_or_init(|| Mutex::new(Instant::now()))
}

fn git_call_stats_key(argv: &[String]) -> String {
    let mut key = String::new();
    let mut i = 0usize;
    while i < argv.len() && key.len() < 160 {
        if argv[i] == "-c" {
            i += 2;
            continue;
        }
        key.push_str(&argv[i]);
        key.push(' ');
        i += 1;
    }
    if i < argv.len() {
        key.push_str("...");
    }
    key
}

fn git_call_stats_add_ms(argv: &[String], elapsed_ms: u64) {
    if elapsed_ms == 0 {
        return;
    }
    let key = git_call_stats_key(argv);
    let mut ms = match git_call_stats_ms().lock() {
        Ok(ms) => ms,
        Err(poisoned) => poisoned.into_inner(),
    };
    if ms.len() < GIT_CALL_STATS_MAX_KEYS || ms.contains_key(&key) {
        *ms.entry(key).or_insert(0) += elapsed_ms;
    }
}

pub fn git_call_stats_note(argv: &[String], reached: bool, elapsed_ms: u64) {
    let key = git_call_stats_key(argv);
    git_call_stats_add_ms(argv, elapsed_ms);
    {
        let mut counts = match git_call_stats_counts(reached).lock() {
            Ok(counts) => counts,
            Err(poisoned) => poisoned.into_inner(),
        };
        if counts.len() < GIT_CALL_STATS_MAX_KEYS || counts.contains_key(&key) {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    let mut last_flush = match git_call_stats_last_flush().lock() {
        Ok(last) => last,
        Err(poisoned) => poisoned.into_inner(),
    };
    if last_flush.elapsed() < GIT_CALL_STATS_FLUSH_INTERVAL {
        return;
    }
    *last_flush = Instant::now();
    let mut asked = match git_call_stats_counts(false).lock() {
        Ok(counts) => counts,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut reached_counts = match git_call_stats_counts(true).lock() {
        Ok(counts) => counts,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut ms_by_key = match git_call_stats_ms().lock() {
        Ok(ms) => ms,
        Err(poisoned) => poisoned.into_inner(),
    };
    if asked.is_empty() && reached_counts.is_empty() {
        return;
    }
    let asked_total: usize = asked.values().sum();
    let reached_total: usize = reached_counts.values().sum();
    let ms_total: u64 = ms_by_key.values().sum();
    eprintln!(
        "[agentplug host] git calls this minute: {asked_total} asked, {reached_total} spawned a git process, {ms_total}ms of wall time in them, {} answered without spawning git",
        asked_total.saturating_sub(reached_total)
    );
    let mut ranked: Vec<(String, usize)> = asked
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    for (key, count) in reached_counts.iter() {
        if !asked.contains_key(key) {
            ranked.push((key.clone(), *count));
        }
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, count) in ranked.into_iter().take(8) {
        let reached = reached_counts.remove(&key).unwrap_or(0);
        let ms = ms_by_key.remove(&key).unwrap_or(0);
        eprintln!(
            "[agentplug host]   asked {count:>5} spawned {reached:>5} {ms:>7}ms : git {key}"
        );
    }
    reached_counts.clear();
    ms_by_key.clear();
    asked.clear();
}

#[derive(serde::Deserialize)]
struct CapabilityAllowlistConfig {
    #[serde(flatten)]
    allow: HashMap<String, Vec<String>>,
}

fn compiled_default_capability_allowlist(caller_plugin: &str, callee_plugin: &str) -> bool {
    match caller_plugin {
        "gm" => matches!(callee_plugin, "bert" | "libsql" | "treesitter" | "crux"),
        _ => false,
    }
}

fn load_capability_allowlist_config(cwd: &Path) -> Option<CapabilityAllowlistConfig> {
    let path = cwd.join(".agentplug").join("capability-allowlist.json");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<CapabilityAllowlistConfig>(&s).ok())
}

pub(crate) fn plugin_call_capability_allowed(cwd: &Path, caller_plugin: &str, callee_plugin: &str) -> bool {
    match load_capability_allowlist_config(cwd) {
        Some(cfg) => cfg
            .allow
            .get(caller_plugin)
            .map(|allowed| allowed.iter().any(|p| p == callee_plugin))
            .unwrap_or(false),
        None => compiled_default_capability_allowlist(caller_plugin, callee_plugin),
    }
}

fn is_well_formed_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_well_formed_verb(verb: &str) -> bool {
    !verb.is_empty()
        && verb.len() <= 128
        && verb
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub(crate) fn validate_plugin_call_body(body: &str) -> Result<(), String> {
    if body.is_empty() {
        return Ok(());
    }
    if body.len() > 64 * 1024 * 1024 {
        return Err("body_exceeds_max_size".to_string());
    }
    serde_json::from_str::<serde_json::Value>(body)
        .map(|_| ())
        .map_err(|e| format!("body_not_valid_json: {e}"))
}

pub fn git_subprocess_timeout_ms() -> u64 {
    std::env::var("AGENTPLUG_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(GIT_SUBPROCESS_TIMEOUT_MS_DEFAULT_AMPLE_FOR_SLOW_PUSH_FETCH_OR_FIRST_CLONE)
}

const GIT_PUSH_SUBPROCESS_TIMEOUT_MS_DEFAULT: u64 = 120_000;

/// Budget for one git child. A push is capped at 120 s (AGENTPLUG_GIT_PUSH_TIMEOUT_MS overrides it), so a hung push
/// releases the git lane sooner; clone and fetch keep the full git budget.
pub fn git_subprocess_timeout_ms_for(argv: &[String]) -> u64 {
    if git_argv_subcommand(argv) == Some("fetch") {
        return git_subprocess_timeout_ms().max(600_000);
    }
    if git_argv_subcommand(argv) != Some("push") {
        return git_subprocess_timeout_ms();
    }
    std::env::var("AGENTPLUG_GIT_PUSH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or_else(|| git_subprocess_timeout_ms().min(GIT_PUSH_SUBPROCESS_TIMEOUT_MS_DEFAULT))
}

const HOST_GIT_STDOUT_CAP_BYTES: usize = 1_048_576;
const HOST_GIT_STDERR_CAP_BYTES: usize = 262_144;

pub(crate) fn drain_reader_capped<R: std::io::Read + Send + 'static>(
    mut reader: R,
    cap: usize,
    cap_hit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(buf.len());
                    if room == 0 {
                        truncated = true;
                        cap_hit.store(true, Ordering::SeqCst);
                        break;
                    }
                    let take = room.min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                        cap_hit.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        (buf, truncated)
    })
}

pub(crate) fn normalize_lexically(path: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }

    Some(out)
}

fn github_cli_config_dir_slot() -> &'static Mutex<Option<PathBuf>> {
    static SLOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_github_cli_config_dir(directory: Option<PathBuf>) {
    *github_cli_config_dir_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = directory;
}

pub fn github_cli_config_dir() -> Option<PathBuf> {
    github_cli_config_dir_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(windows)]
const EMPTY_GIT_CONFIG_OR_HOOK_PATH: &str = "NUL";

#[cfg(not(windows))]
const EMPTY_GIT_CONFIG_OR_HOOK_PATH: &str = "/dev/null";

fn is_inherited_git_config_key(key: &std::ffi::OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    matches!(
        key.as_str(),
        "GIT_CONFIG_COUNT"
            | "GIT_CONFIG_GLOBAL"
            | "GIT_CONFIG_NOSYSTEM"
            | "GIT_CONFIG_PARAMETERS"
            | "GIT_CONFIG_SYSTEM"
    ) || key.starts_with("GIT_CONFIG_KEY_")
        || key.starts_with("GIT_CONFIG_VALUE_")
}

const GIT_SUBCOMMANDS_APPLYING_CONTENT_FILTERS: &[&str] = &[
    "add", "status", "diff", "checkout", "commit", "stash",
];

const GIT_GLOBAL_OPTIONS_TAKING_A_VALUE: &[&str] = &[
    "-c",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
    "--exec-path",
];

fn git_argv_subcommand(argv: &[String]) -> Option<&str> {
    let mut tokens = argv.iter().map(String::as_str);
    while let Some(token) = tokens.next() {
        if GIT_GLOBAL_OPTIONS_TAKING_A_VALUE.contains(&token) {
            tokens.next();
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return Some(token);
    }
    None
}

fn git_argv_applies_content_filters(argv: &[String]) -> bool {
    git_argv_subcommand(argv)
        .is_some_and(|subcommand| GIT_SUBCOMMANDS_APPLYING_CONTENT_FILTERS.contains(&subcommand))
}

pub(crate) fn configure_github_git_credentials(command: &mut std::process::Command, argv: &[String]) {
    for (key, _) in std::env::vars_os() {
        if is_inherited_git_config_key(&key) {
            command.env_remove(key);
        }
    }
    if let Some(directory) = github_cli_config_dir() {
        command.env("GH_CONFIG_DIR", directory);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", EMPTY_GIT_CONFIG_OR_HOOK_PATH)
        .env("GIT_CONFIG_KEY_1", "credential.https://github.com.helper")
        .env("GIT_CONFIG_VALUE_1", "!gh auth git-credential");
    if git_argv_applies_content_filters(argv) {
        return;
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", EMPTY_GIT_CONFIG_OR_HOOK_PATH);
}

fn user_gm_root() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())?;
    let home = home.trim();
    if home.is_empty() {
        return None;
    }
    normalize_lexically(&std::path::Path::new(home).join(".gm"))
}

static FS_WRITE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, std::sync::Arc<Mutex<()>>>>> =
    OnceLock::new();

fn fs_write_lock_for(path: &Path) -> std::sync::Arc<Mutex<()>> {
    let key = canonicalize_path_separators_for_stable_keying(path);
    let registry = FS_WRITE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

pub(crate) fn atomic_write_locked(full: &Path, data: &str) -> std::io::Result<()> {
    let lock = fs_write_lock_for(full);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    atomic_write_under_held_lock(full, data)
}

// Every path this writer touches gets a {name}.tmp-{pid}-{seq} sibling, removed only on this function's own
// failure paths. A process killed between create_private_file and rename leaves that sibling forever, so this
// writer sweeps them itself: nothing else reaches paths outside .gm/exec-spool, where the guest's
// dispatch_ledger::sweep_stale_spool_tmp cannot look. The candidate rule and the staleness window are the ones
// rs-plugkit's dispatch_ledger::sweep_stale_tmp_beside uses, so one bound covers temps from either side.
const TMP_SWEEP_MARKER: &str = ".tmp-";
const TMP_SWEEP_STALE_MS: u64 = 30 * 1000;

fn sweep_stale_tmp_beside(full: &Path, name: &str) {
    let dir = match full.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => return,
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for entry in entries.flatten() {
        let Some(candidate) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !(candidate.starts_with(name) && candidate.contains(TMP_SWEEP_MARKER)) {
            continue;
        }
        // DirEntry::metadata does not follow a symlink, so this also refuses to remove one.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        if mtime_ms.is_some_and(|ms| ms.saturating_add(TMP_SWEEP_STALE_MS) < now_ms) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn atomic_write_under_held_lock(full: &Path, data: &str) -> std::io::Result<()> {
    let name = full.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    // Before the identical-content shortcut: a path rewritten with the bytes it already holds is the common
    // no-op case, and it still owns any temp left beside it by a process that died.
    sweep_stale_tmp_beside(full, name);
    if fs::read(full).is_ok_and(|existing| existing == data.as_bytes()) {
        return Ok(());
    }
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = full.with_file_name(format!("{name}.tmp-{}-{seq}", std::process::id()));
    if let Err(e) = create_private_file(&tmp, data) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    match fs::rename(&tmp, full) {
        Ok(()) => {
            if let Some(dir) = full.parent() {
                if let Ok(d) = fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(unix)]
pub(crate) fn create_private_file(path: &Path, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data.as_bytes())
}

#[cfg(not(unix))]
pub(crate) fn create_private_file(path: &Path, data: &str) -> std::io::Result<()> {
    fs::write(path, data)
}

pub(crate) fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}


// host_fs_write and host_fs_cas_write return 1 on success, 2 for a CAS mismatch and 0 for every other
// failure. A full volume is the one failure a caller can act on (retry later, free space, stop appending),
// so it gets its own code, 3, instead of collapsing into 0 with a sandbox refusal. The guest still folds
// unknown codes to IoError until it maps 3, which is why the path is named on stderr too.
pub(crate) const HOST_WRITE_NO_SPACE: u32 = 3;

pub(crate) fn io_error_is_out_of_space(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::StorageFull {
        return true;
    }
    // ENOSPC, ERROR_DISK_FULL (112) and ERROR_HANDLE_DISK_FULL (39); Rust reports StorageFull only when
    // it maps the errno itself, which it does not do for every platform or every write path.
    matches!(err.raw_os_error(), Some(28) | Some(112) | Some(39))
}

fn classify_write_error(err: &std::io::Error, full: &std::path::Path, what: &str) -> u32 {
    if io_error_is_out_of_space(err) {
        eprintln!(
            "[agentplug host] {what} for {} failed with no space left on the target filesystem (os error {:?}) -- the temporary file was removed, so no partial content is visible at that path",
            full.display(),
            err.raw_os_error()
        );
        return HOST_WRITE_NO_SPACE;
    }
    0
}

pub(crate) fn atomic_cas_write_locked(full: &Path, expected: &str, data: &str) -> std::io::Result<bool> {
    let lock = fs_write_lock_for(full);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let current = fs::read_to_string(full).unwrap_or_default();
    if current != expected {
        return Ok(false);
    }
    atomic_write_under_held_lock(full, data)?;
    Ok(true)
}

pub(crate) fn has_project_marker(dir: &std::path::Path) -> bool {
    const MARKERS: &[&str] = &[
        ".git",
        ".gm",
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
    ];
    MARKERS.iter().any(|m| dir.join(m).exists())
}

pub(crate) fn sandboxed_guest_path_with_extra_roots(
    cwd: &std::path::Path,
    path: &str,
    extra_roots: &[PathBuf],
) -> Option<PathBuf> {
    let requested = std::path::Path::new(path);
    let joined = if requested.is_absolute() || requested.has_root() {
        requested.to_path_buf()
    } else {
        cwd.join(requested)
    };
    let normalized = normalize_lexically(&joined)?;
    let root = normalize_lexically(cwd)?;
    if normalized == root || normalized.starts_with(&root) {
        return Some(normalized);
    }
    let user_root = user_gm_root()?;
    if normalized.starts_with(&user_root) {
        return Some(normalized);
    }
    for extra in extra_roots {
        let Some(extra_normalized) = normalize_lexically(extra) else {
            continue;
        };
        if normalized == extra_normalized || normalized.starts_with(&extra_normalized) {
            return Some(normalized);
        }
    }
    None
}

fn canonicalize_path_separators_for_stable_keying(path: &std::path::Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Memory {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("wasm module did not export linear memory")
}

fn read_guest_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    let memory = guest_memory(caller);
    let mut buf = vec![0u8; len as usize];
    let _ = memory.read(&mut *caller, ptr as usize, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

const PACKED_ABI_FIELD_MAX: usize = 0xffff_ffff;

fn pack_guest_ptr_len(ptr: u32, len: usize) -> u64 {
    debug_assert!(
        len <= PACKED_ABI_FIELD_MAX,
        "write_guest_bytes: length {len} exceeds the 32-bit field of the packed (ptr,len) ABI"
    );
    (ptr as u64 & 0xffff_ffff) | ((len as u64) << 32)
}

fn write_guest_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    if bytes.len() > PACKED_ABI_FIELD_MAX {
        let reason = format!(
            "host response of {} bytes exceeds the 32-bit packed-ABI field",
            bytes.len()
        );
        eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
        caller.data().note_lost_response(reason);
        return 0;
    }
    let instance = caller
        .data()
        .self_instance
        .lock()
        .unwrap()
        .expect("instance not yet bound to host state");
    let alloc = instance
        .get_typed_func::<u32, u32>(&mut *caller, "plugkit_alloc")
        .expect("plugkit_alloc export missing on wasm module");
    const RESPONSE_HANDOFF_GRACE_SECS: u64 = 5;
    let real_deadline_secs = caller.data().call_deadline_secs();
    caller
        .as_context_mut()
        .set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(
            RESPONSE_HANDOFF_GRACE_SECS,
        ));
    match alloc.call(&mut *caller, bytes.len() as u32) {
        Ok(ptr) => {
            let memory = guest_memory(caller);
            if memory.write(&mut *caller, ptr as usize, bytes).is_err() {
                let reason = format!(
                    "guest memory.write of {} bytes at ptr {ptr} failed",
                    bytes.len()
                );
                eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
                caller.data().note_lost_response(reason);
                caller.as_context_mut().set_epoch_deadline(
                    crate::registry::epoch_ticks_for_seconds(real_deadline_secs),
                );
                return 0;
            }
            caller
                .as_context_mut()
                .set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(real_deadline_secs));
            pack_guest_ptr_len(ptr, bytes.len())
        }
        Err(e) => {
            let interrupted = matches!(
                e.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::Interrupt)
            );
            let reason = if interrupted {
                format!("plugkit_alloc({}) hit the epoch deadline while handing the host response back to the guest", bytes.len())
            } else {
                format!("plugkit_alloc({}) failed: {e}", bytes.len())
            };
            eprintln!("[agentplug host] write_guest_bytes: {reason} -- returning 0, which the guest reads as a null response");
            caller.data().note_lost_response(reason);
            caller
                .as_context_mut()
                .set_epoch_deadline(crate::registry::epoch_ticks_for_seconds(real_deadline_secs));
            0
        }
    }
}

fn write_guest_json(caller: &mut Caller<'_, HostState>, v: serde_json::Value) -> u64 {
    write_guest_bytes(caller, v.to_string().as_bytes())
}

pub fn register_wasi(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    wasmtime_wasi::p1::add_to_linker_sync(linker, |s: &mut HostState| &mut s.wasi)?;
    Ok(())
}

pub(crate) fn embed_text_via_bert(cwd: &Path, caller_plugin: &str, caller_siblings: &crate::registry::SiblingPools, text: &str) -> Option<Vec<f32>> {
    if text.is_empty() || text.len() > 1024 * 1024 {
        return None;
    }
    if !plugin_call_capability_allowed(cwd, caller_plugin, "bert") {
        eprintln!("[agentplug:host_vec_embed] capability_denied: caller {caller_plugin} not permitted to reach bert");
        return None;
    }
    let body = serde_json::json!({"text": text}).to_string();
    let sibling_pool = { caller_siblings.lock().unwrap_or_else(|e| e.into_inner()).get("bert").cloned() };
    let sibling_pool = match sibling_pool {
        Some(pool) => Some(pool),
        None if crate::registry::ensure_sibling_registered(cwd, "bert", caller_siblings) => {
            caller_siblings.lock().unwrap_or_else(|e| e.into_inner()).get("bert").cloned()
        }
        None => {
            let registered: Vec<String> = caller_siblings.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
            eprintln!(
                "[agentplug:host_vec_embed] no `bert` sibling registered for {} (caller plugin {caller_plugin}); this process has {registered:?} loaded -- the embedder is unreachable, so every embedding-dependent verb will fail until bert is loaded into the SAME siblings map as the caller",
                cwd.display()
            );
            None
        }
    };
    let sibling_pool = sibling_pool?;
    const EMBED_RETRY_ATTEMPTS: u32 = 3;
    const EMBED_RETRY_BACKOFF_MS: u64 = 500;
    let mut result: anyhow::Result<Vec<f32>> = Err(anyhow::anyhow!("embed not attempted"));
    for attempt in 0..EMBED_RETRY_ATTEMPTS {
        let mut guard = sibling_pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies");
        result = match guard.as_mut() {
            None => Err(anyhow::anyhow!("bert not loaded yet")),
            Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, "embed", &body, cwd, caller_siblings.clone()).and_then(|resp| {
                let response_json: serde_json::Value = serde_json::from_str(&resp)?;
                let embedding_array = response_json.get("embedding").and_then(|e| e.as_array()).ok_or_else(|| anyhow::anyhow!("no embedding field"))?;
                Ok::<Vec<f32>, anyhow::Error>(embedding_array.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect())
            }),
        };
        if result.is_err() {
            *guard = None;
        } else {
            sibling_pool.evict_if_swap_pending(&mut guard);
        }
        drop(guard);
        if result.is_ok() {
            break;
        }
        if attempt + 1 < EMBED_RETRY_ATTEMPTS {
            sibling_pool.any_instantiated_within(EMBED_RETRY_BACKOFF_MS);
        }
    }
    match result {
        Ok(values) => Some(values),
        Err(e) => {
            eprintln!("[agentplug:host_vec_embed] failed after {EMBED_RETRY_ATTEMPTS} attempts: {e}");
            None
        }
    }
}

pub(crate) fn plugin_call_reply(cwd: &Path, caller_plugin: &str, caller_siblings: &crate::registry::SiblingPools, plugin: &str, verb: &str, body: &str) -> String {
    if !is_well_formed_plugin_name(plugin) {
        return serde_json::json!({"ok": false, "error": "invalid_plugin_name", "plugin": plugin}).to_string();
    }
    if !is_well_formed_verb(verb) {
        return serde_json::json!({"ok": false, "error": "invalid_verb", "verb": verb}).to_string();
    }
    if let Err(reason) = validate_plugin_call_body(body) {
        return serde_json::json!({"ok": false, "error": "invalid_body", "reason": reason}).to_string();
    }
    if !plugin_call_capability_allowed(cwd, caller_plugin, plugin) {
        return serde_json::json!({"ok": false, "error": "capability_denied", "caller": caller_plugin, "plugin": plugin}).to_string();
    }
    let sibling_pool = { caller_siblings.lock().unwrap().get(plugin).cloned() };
    let sibling_pool = match sibling_pool {
        Some(pool) => Some(pool),
        None if crate::registry::ensure_sibling_registered(cwd, plugin, caller_siblings) => {
            caller_siblings.lock().unwrap().get(plugin).cloned()
        }
        None => None,
    };
    let Some(sibling_pool) = sibling_pool else {
        return serde_json::json!({"ok": false, "error": "unknown_plugin", "plugin": plugin}).to_string();
    };
    let acquire_start = std::time::Instant::now();
    let acquire_timeout_ms = crate::registry::SharedPluginPool::ACQUIRE_TIMEOUT_MS;
    let mut guard = sibling_pool.acquire().expect("acquire() always returns Some -- FIFO wait never denies");
    if guard.is_none() {
        drop(guard);
        crate::registry::ensure_sibling_registered(cwd, plugin, caller_siblings);
        let remaining_ms = acquire_timeout_ms.saturating_sub(acquire_start.elapsed().as_millis() as u64);
        sibling_pool.any_instantiated_within(remaining_ms);
        let remaining_ms = acquire_timeout_ms.saturating_sub(acquire_start.elapsed().as_millis() as u64);
        guard = sibling_pool.acquire_within(remaining_ms).0;
    }
    let result = match guard.as_mut() {
        None => Err(anyhow::anyhow!("plugin_not_loaded_yet")),
        Some(handle) => crate::registry::dispatch_on(&mut handle.store, handle.instance, verb, body, cwd, caller_siblings.clone()),
    };
    sibling_pool.evict_if_swap_pending(&mut guard);
    drop(guard);
    match result {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => serde_json::json!({"ok": true}).to_string(),
        Err(e) if e.to_string() == "plugin_not_loaded_yet" => serde_json::json!({"ok": false, "error": "plugin_not_loaded_yet", "plugin": plugin}).to_string(),
        Err(e) => serde_json::json!({"ok": false, "error": e.to_string(), "plugin": plugin}).to_string(),
    }
}

pub(crate) fn host_git_reply(root: &Path, args: &str, cwd_arg: &str) -> serde_json::Value {
            let trimmed = args.trim();
            let argv: Vec<String> = if trimmed.starts_with('[') {
                serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_else(|_| trimmed.split_whitespace().map(String::from).collect())
            } else {
                trimmed.split_whitespace().map(String::from).collect()
            };
            let cwd = if cwd_arg.is_empty() {
                root.to_path_buf()
            } else {
                let cwd_path = PathBuf::from(cwd_arg);
                if cwd_path.is_absolute() { cwd_path } else { root.to_path_buf().join(cwd_path) }
            };
            if !cwd.is_dir() {
                let error_json = serde_json::json!({
                    "stdout": "",
                    "stderr": format!(
                        "git cwd does not exist: {} (resolved from cwd arg {:?}) -- pass an absolute path, or a path relative to the dispatch's own working directory, not a bare project/repo name",
                        cwd.display(), cwd_arg
                    ),
                    "exit_code": -1,
                });
                return error_json;
                }
                    git_call_stats_note(&argv, false, 0);
                    let layout_question = git_layout_question(&cwd, &argv);
                    let index_question = git_index_question(&cwd, &argv);
                    let index_identity =
                        index_question.as_ref().and_then(|(dir, _)| git_index_identity(dir));
                    if let Some((dir, question)) = layout_question.as_ref() {
                        if let Some(answer) = git_layout_recall(dir, question, None) {
                            return serde_json::json!({
                                "stdout": answer.stdout,
                                "stderr": answer.stderr,
                                "exit_code": answer.exit_code,
                                "stdout_truncated": false,
                                "stderr_truncated": false,
                            });
                        }
                    }
                    if let Some((dir, question)) = index_question.as_ref() {
                        if let Some(answer) = git_layout_recall(dir, question, index_identity.clone()) {
                            return serde_json::json!({
                                "stdout": answer.stdout,
                                "stderr": answer.stderr,
                                "exit_code": answer.exit_code,
                                "stdout_truncated": false,
                                "stderr_truncated": false,
                            });
                        }
                    }
                    if let Some(dir) = git_head_question(&cwd, &argv) {
                        if let Some(sha) = git_head_sha_from_disk(&dir) {
                            return serde_json::json!({
                                "stdout": format!("{sha}\n"),
                                "stderr": "",
                                "exit_code": 0,
                                "stdout_truncated": false,
                                "stderr_truncated": false,
                            });
                        }
                    }
                    git_call_stats_note(&argv, true, 0);
                    let git_started = Instant::now();
                    let mut git_cmd = std::process::Command::new("git");
                    git_cmd.args(&argv).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                    configure_github_git_credentials(&mut git_cmd, &argv);
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    git_cmd.process_group(0);
                }
                #[cfg(windows)]
                {
                    crate::windowless::apply_windowless(&mut git_cmd);
                }
                let spawn_result = match git_cmd.spawn() {
                    Ok(mut child) => {
                        let containment = match crate::process_tree::establish_containment(&child) {
                            Ok(containment) => containment,
                            Err(error) => {
                                crate::process_tree::kill_tree(child.id());
                                let _ = child.kill();
                                let _ = child.wait();
                                return serde_json::json!({
                                    "stdout": "",
                                    "stderr": format!("git process containment failed: {error}"),
                                    "exit_code": -1,
                                });
                            }
                        };
                        let cap_hit = Arc::new(AtomicBool::new(false));
                    let out_handle = child.stdout.take().map(|o| drain_reader_capped(o, HOST_GIT_STDOUT_CAP_BYTES, cap_hit.clone()));
                    let err_handle = child.stderr.take().map(|e| drain_reader_capped(e, HOST_GIT_STDERR_CAP_BYTES, cap_hit.clone()));
                    let timeout_ms = git_subprocess_timeout_ms_for(&argv);
                    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                    let mut exit_code: i32 = -1;
                    let mut timed_out = false;
                    let mut cap_stopped = false;
                    loop {
                            if cap_hit.load(Ordering::SeqCst) {
                                crate::process_tree::terminate_containment(&containment, child.id());
                                let _ = child.wait();
                            cap_stopped = true;
                            break;
                        }
                        match child.wait_timeout(Duration::from_millis(20)) {
                            Ok(Some(status)) => { exit_code = status.code().unwrap_or(-1); break; }
                            Ok(None) => {}
                            Err(_) => break,
                        }
                            if Instant::now() >= deadline {
                                crate::process_tree::terminate_containment(&containment, child.id());
                                let _ = child.wait();
                            timed_out = true;
                            break;
                        }
                        }
                        let (stdout, out_trunc_read) =
                            out_handle.and_then(|h| h.join().ok()).unwrap_or_default();
                        let (stderr, err_trunc_read) =
                            err_handle.and_then(|h| h.join().ok()).unwrap_or_default();
                        let output_truncated = out_trunc_read || err_trunc_read || cap_stopped;
                        let stderr = String::from_utf8_lossy(&stderr);
                        let output_limit_error =
                            format!("git {argv:?} output exceeded the host capture limit");
                        if let Some((dir, question)) = layout_question.as_ref() {
                            if timed_out || output_truncated || exit_code != 0 {
                                git_layout_forget_dir(dir);
                            } else {
                                git_layout_remember(
                                    dir.clone(),
                                    question,
                                    GitLayoutAnswer {
                                        stdout: String::from_utf8_lossy(&stdout).into_owned(),
                                        stderr: stderr.to_string(),
                                        exit_code,
                                        fingerprint: None,
                                    },
                                );
                            }
                        } else if let Some((dir, question)) = index_question.as_ref() {
                            if timed_out || output_truncated || exit_code != 0 {
                                git_layout_forget_dir(dir);
                            } else if let Some(identity) = index_identity.as_ref() {
                                if stdout.len() <= GIT_INDEX_ANSWER_MAX_BYTES {
                                    git_layout_remember(
                                        dir.clone(),
                                        question,
                                        GitLayoutAnswer {
                                            stdout: String::from_utf8_lossy(&stdout).into_owned(),
                                            stderr: stderr.to_string(),
                                            exit_code,
                                            fingerprint: Some(identity.clone()),
                                        },
                                    );
                                }
                            }
                        } else {
                            git_layout_forget_dir(&cwd);
                        }
                        if timed_out {
                            serde_json::json!({
                                "stdout": String::from_utf8_lossy(&stdout),
                                "stderr": format!("git {argv:?} timed out after {timeout_ms}ms, killed"),
                                "exit_code": -1,
                            })
                        } else {
                            serde_json::json!({
                                "stdout": String::from_utf8_lossy(&stdout),
                                "stderr": if output_truncated {
                                    if stderr.is_empty() {
                                        output_limit_error
                                    } else {
                                        format!("{stderr}\n{output_limit_error}")
                                    }
                                } else {
                                    stderr.into_owned()
                                },
                                "exit_code": if output_truncated { -1 } else { exit_code },
                                "stdout_truncated": out_trunc_read,
                                "stderr_truncated": err_trunc_read,
                            })
                        }
                }
                Err(e) => serde_json::json!({"stdout": "", "stderr": e.to_string(), "exit_code": 1}),
            };
            git_call_stats_add_ms(&argv, git_started.elapsed().as_millis() as u64);
            if std::env::var_os("GM_GIT_TRACE").is_some() {
                eprintln!(
                    "[agentplug:git_trace] argv={argv:?} cwd={} elapsed_ms={:.1}",
                    cwd.display(),
                    git_started.elapsed().as_secs_f64() * 1000.0
                );
            }
            spawn_result
}

pub fn register_env_imports(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    linker.func_wrap(
        "env",
        "host_cwd",
        |mut caller: Caller<'_, HostState>| -> u64 {
            let cwd = caller.data().cwd().to_string_lossy().into_owned();
            write_guest_bytes(&mut caller, cwd.as_bytes())
        },
    )?;
    linker.func_wrap(
        "env",
        "host_fs_allow_root",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let Some(normalized) = normalize_lexically(std::path::Path::new(&path)) else {
                return 0;
            };
            match fs::metadata(&normalized) {
                Ok(md) if md.is_dir() && has_project_marker(&normalized) => {
                    caller.data().allow_extra_root(normalized);
                    1
                }
                _ => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_fs_read",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            match fs::read_to_string(&full) {
                Ok(content) if content.is_empty() => HOST_FS_READ_EMPTY_SUCCESS,
                Ok(content) => write_guest_bytes(&mut caller, content.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_read_base64",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            use std::io::Read;
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            let Ok(file) = fs::File::open(&full) else {
                return 0;
            };
            let mut bytes = Vec::new();
            if file
                .take(HOST_BINARY_READ_MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.is_empty()
                || bytes.len() as u64 > HOST_BINARY_READ_MAX_BYTES
            {
                return 0;
            }
            write_guest_bytes(&mut caller, base64_encode(&bytes).as_bytes())
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_write",
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         data_ptr: u32,
         data_len: u32|
         -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let data = read_guest_string(&mut caller, data_ptr, data_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match atomic_write_locked(&full, &data) {
                Ok(()) => 1,
                Err(err) => classify_write_error(&err, &full, "host_fs_write"),
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_cas_write",
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         expected_ptr: u32,
         expected_len: u32,
         data_ptr: u32,
         data_len: u32|
         -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let expected = read_guest_string(&mut caller, expected_ptr, expected_len);
            let data = read_guest_string(&mut caller, data_ptr, data_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            if let Some(parent) = full.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match atomic_cas_write_locked(&full, &expected, &data) {
                Ok(true) => 1,
                Ok(false) => 2,
                Err(err) => classify_write_error(&err, &full, "host_fs_cas_write"),
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_remove",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u32 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            match fs::metadata(&full) {
                Ok(md) if md.is_dir() => 0,
                Ok(_) => match fs::remove_file(&full) {
                    Ok(()) => 1,
                    Err(_) => 0,
                },
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_readdir",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) =
                sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots)
            else {
                return 0;
            };
            let Ok(entries) = fs::read_dir(&full).and_then(|rd| {
                rd.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                    .collect::<std::io::Result<Vec<String>>>()
            }) else {
                return 0;
            };
            write_guest_json(&mut caller, serde_json::json!(entries))
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fs_stat",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32| -> u64 {
            let path = read_guest_string(&mut caller, path_ptr, path_len);
            let extra_roots = caller.data().extra_readable_roots();
            let Some(full) = sandboxed_guest_path_with_extra_roots(&caller.data().cwd(), &path, &extra_roots) else { return 0 };
            match fs::metadata(&full) {
                Ok(md) => {
                    let mtime_ms = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let canonical_path = fs::canonicalize(&full)
                        .ok()
                        .map(|path| path.to_string_lossy().into_owned());
                    let metadata_json = serde_json::json!({"isDirectory": md.is_dir(), "isFile": md.is_file(), "size": md.len(), "mtimeMs": mtime_ms, "mtime_ms": mtime_ms, "canonicalPath": canonical_path});
                    write_guest_json(&mut caller, metadata_json)
                }
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_now_ms",
        |_caller: Caller<'_, HostState>| -> u64 {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_log",
        |mut caller: Caller<'_, HostState>, level: u32, msg_ptr: u32, msg_len: u32| -> u32 {
            let msg = read_guest_string(&mut caller, msg_ptr, msg_len);
            let plugin = caller.data().plugin_name.clone();
            if let Some(evt_line) = msg.strip_prefix("evt: ") {
                let cwd = caller.data().cwd.lock().unwrap().clone();
                crate::watcher_log::append_watcher_line(&cwd, &format!("evt: {evt_line}"));
                return 1;
            }
            eprintln!("[agentplug:{plugin} L{level}] {msg}");
            1
        },
    )?;

    linker.func_wrap(
        "env",
        "host_env_get",
        |mut caller: Caller<'_, HostState>, key_ptr: u32, key_len: u32| -> u64 {
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            if let Some(dispatch_value) = crate::dispatch_origin::dispatch_env_value(&key) {
                return match dispatch_value {
                    Some(val) => write_guest_bytes(&mut caller, val.as_bytes()),
                    None => 0,
                };
            }
            match std::env::var(&key) {
                Ok(val) => write_guest_bytes(&mut caller, val.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "host_random_fill",
        |mut caller: Caller<'_, HostState>, ptr: u32, len: u32| -> u32 {
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut buf = vec![0u8; len as usize];
            let mut seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15)
                ^ (std::process::id() as u64).wrapping_mul(0xBF58476D1CE4E5B9);
            for byte in buf.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = (seed & 0xff) as u8;
            }
            let memory = guest_memory(&mut caller);
            if memory.write(&mut caller, ptr as usize, &buf).is_err() {
                return 0;
            }
            1
        },
    )?;

    linker.func_wrap(
        "env",
        "host_fetch",
        |mut caller: Caller<'_, HostState>,
         url_ptr: u32,
         url_len: u32,
         opts_ptr: u32,
         opts_len: u32|
         -> u64 {
            let url = read_guest_string(&mut caller, url_ptr, url_len);
            let opts_str = read_guest_string(&mut caller, opts_ptr, opts_len);
            let opts: serde_json::Value = if opts_str.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&opts_str).unwrap_or(serde_json::json!({}))
            };
            let method = opts
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();
            let body = opts.get("body").and_then(|v| v.as_str());
            let timeout = opts
                .get("timeoutMs")
                .and_then(|v| v.as_u64())
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_secs(10));
            let binary_response = opts.get("responseEncoding").and_then(|v| v.as_str()) == Some("base64");
            let fetch_agent = if binary_response {
                crate::http_agent::build_agent_without_redirects(timeout)
            } else {
                fetch_agent(timeout)
            };
            let mut req = fetch_agent.request(&method, &url);
            if let Some(headers) = opts.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in headers {
                    if let Some(vs) = v.as_str() {
                        req = req.set(k, vs);
                    }
                }
            }
            req = req.timeout(timeout);
            let resp = match body {
                Some(b) => req.send_string(b),
                None => req.call(),
            };
            let result = match resp {
                Ok(r) if binary_response => {
                    use std::io::Read;
                    let status = r.status();
                    let content_type = r.header("content-type").unwrap_or("").to_string();
                    let location = r.header("location").map(String::from);
                    let mut bytes = Vec::new();
                    match r
                        .into_reader()
                        .take(HOST_BINARY_READ_MAX_BYTES + 1)
                        .read_to_end(&mut bytes)
                    {
                        Ok(_) if bytes.len() as u64 > HOST_BINARY_READ_MAX_BYTES => serde_json::json!({"ok": false, "status": status, "error": "response_exceeds_binary_limit", "limit_bytes": HOST_BINARY_READ_MAX_BYTES}),
                        Ok(_) => serde_json::json!({"ok": true, "status": status, "content_type": content_type, "location": location, "body": base64_encode(&bytes), "encoding": "base64"}),
                        Err(e) => serde_json::json!({"ok": false, "status": status, "error": e.to_string()}),
                    }
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"ok": true, "status": status, "body": text})
                }
                Err(ureq::Error::Status(code, r)) => {
                    let location = r.header("location").map(String::from);
                    let text = r.into_string().unwrap_or_default();
                    serde_json::json!({"ok": false, "status": code, "body": text, "location": location})
                }
                Err(e) => serde_json::json!({"ok": false, "status": 0, "error": e.to_string()}),
            };
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_kv_get",
        |mut caller: Caller<'_, HostState>,
         ns_ptr: u32,
         ns_len: u32,
         key_ptr: u32,
         key_len: u32|
         -> u64 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            match fs::read_to_string(&path) {
                Ok(content) => write_guest_bytes(&mut caller, content.as_bytes()),
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_put",
        |mut caller: Caller<'_, HostState>,
         ns_ptr: u32,
         ns_len: u32,
         key_ptr: u32,
         key_len: u32,
         val_ptr: u32,
         val_len: u32|
         -> u32 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            let val = read_guest_string(&mut caller, val_ptr, val_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            if let Some(parent) = path.parent() {
                let _ = create_private_dir_all(parent);
            }
            match create_private_file(&path, &val) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_cas",
        |mut caller: Caller<'_, HostState>,
         ns_ptr: u32,
         ns_len: u32,
         key_ptr: u32,
         key_len: u32,
         expected_ptr: u32,
         expected_len: u32,
         val_ptr: u32,
         val_len: u32|
         -> u32 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            let expected = read_guest_string(&mut caller, expected_ptr, expected_len);
            let val = read_guest_string(&mut caller, val_ptr, val_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            if let Some(parent) = path.parent() {
                let _ = create_private_dir_all(parent);
            }
            // A missing key compares equal to an empty expected value. The per-path lock makes the
            // read-compare-write one step against other host_kv_cas calls on this key; host_kv_put
            // does not take it.
            let lock = fs_write_lock_for(&path);
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let current = fs::read_to_string(&path).unwrap_or_default();
            if current != expected {
                return 0;
            }
            match create_private_file(&path, &val) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_delete",
        |mut caller: Caller<'_, HostState>,
         ns_ptr: u32,
         ns_len: u32,
         key_ptr: u32,
         key_len: u32|
         -> u32 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let key = read_guest_string(&mut caller, key_ptr, key_len);
            if ns.is_empty() || key.is_empty() {
                return 0;
            }
            let path = kv_file_path(&caller.data().cwd(), &ns, &key);
            match fs::remove_file(&path) {
                Ok(()) => 1,
                Err(_) => 0,
            }
        },
    )?;
    linker.func_wrap(
        "env",
        "host_kv_query",
        |mut caller: Caller<'_, HostState>,
         ns_ptr: u32,
         ns_len: u32,
         q_ptr: u32,
         q_len: u32|
         -> u64 {
            let ns = read_guest_string(&mut caller, ns_ptr, ns_len);
            let query = read_guest_string(&mut caller, q_ptr, q_len).to_lowercase();
            if ns.is_empty() {
                return 0;
            }
            let dir = kv_namespace_dir(&caller.data().cwd(), &ns);
            let mut results = Vec::new();
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(content) = fs::read_to_string(&path) {
                        if query.is_empty() || content.to_lowercase().contains(&query) {
                            let key = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or_default()
                                .to_string();
                            results.push(serde_json::json!({"key": key, "value": content}));
                        }
                    }
                }
            }
            write_guest_json(&mut caller, serde_json::json!(results))
        },
    )?;

    linker.func_wrap(
        "env",
        "host_exec_js",
        |mut caller: Caller<'_, HostState>,
         code_ptr: u32,
         code_len: u32,
         opts_ptr: u32,
         opts_len: u32|
         -> u64 {
            let code = read_guest_string(&mut caller, code_ptr, code_len);
            let opts_str = read_guest_string(&mut caller, opts_ptr, opts_len);
            let opts: serde_json::Value = if opts_str.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&opts_str).unwrap_or(serde_json::json!({}))
            };
            let cwd = caller.data().cwd();
            let result = crate::exec_js::run(&code, &opts, &cwd);
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_vec_search",
        |mut caller: Caller<'_, HostState>, q_ptr: u32, q_len: u32, k: u32| -> u64 {
            let _ = (q_ptr, q_len, k);
            write_guest_json(&mut caller, serde_json::json!({"ok": false, "error": "host_vec_search_unused_guest_runs_libsql_directly"}))
        },
    )?;
    linker.func_wrap(
        "env",
        "host_task_proc",
        |mut caller: Caller<'_, HostState>,
         a_ptr: u32,
         a_len: u32,
         p_ptr: u32,
         p_len: u32|
         -> u64 {
            let action = read_guest_string(&mut caller, a_ptr, a_len);
            let params_str = read_guest_string(&mut caller, p_ptr, p_len);
            let params: serde_json::Value = if params_str.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&params_str).unwrap_or(serde_json::json!({}))
            };
            let cwd = caller.data().cwd();
            let result = if action == crate::fs_prewarm::ACTION {
                let extra_roots = caller.data().extra_readable_roots();
                crate::fs_prewarm::run(&params, |p| {
                    sandboxed_guest_path_with_extra_roots(&cwd, p, &extra_roots)
                })
            } else {
                crate::task::handle(&action, &params, &cwd)
            };
            write_guest_json(&mut caller, result)
        },
    )?;

    linker.func_wrap(
        "env",
        "host_plugin_call",
        |mut caller: Caller<'_, HostState>,
         plugin_ptr: u32,
         plugin_len: u32,
         verb_ptr: u32,
         verb_len: u32,
         body_ptr: u32,
         body_len: u32|
         -> u64 {
            let plugin = read_guest_string(&mut caller, plugin_ptr, plugin_len);
            let verb = read_guest_string(&mut caller, verb_ptr, verb_len);
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let caller_plugin = caller.data().plugin_name.clone();
            let caller_root = caller.data().cwd();
            let caller_siblings = caller.data().siblings();
            let reply = plugin_call_reply(&caller_root, &caller_plugin, &caller_siblings, &plugin, &verb, &body);
            write_guest_bytes(&mut caller, reply.as_bytes())
        },
    )?;

    linker.func_wrap(
        "env",
        "host_vec_embed",
        |mut caller: Caller<'_, HostState>, text_ptr: u32, text_len: u32, out_ptr: u32, out_len: u32| -> i32 {
            let text = read_guest_string(&mut caller, text_ptr, text_len);
            let caller_plugin = caller.data().plugin_name.clone();
            let caller_root = caller.data().cwd();
            let caller_siblings = caller.data().siblings();
            let Some(values) = embed_text_via_bert(&caller_root, &caller_plugin, &caller_siblings, &text) else {
                return -1;
            };
            let dim = values.len().min(out_len as usize);
            let mut bytes = Vec::with_capacity(dim * 4);
            for v in &values[..dim] {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            let memory = guest_memory(&mut caller);
            if memory.write(&mut caller, out_ptr as usize, &bytes).is_err() {
                return -1;
            }
            dim as i32
        },
    )?;

    linker.func_wrap(
        "env",
        "host_git",
        |mut caller: Caller<'_, HostState>, args_ptr: u32, args_len: u32, cwd_ptr: u32, cwd_len: u32| -> u64 {
            let args = read_guest_string(&mut caller, args_ptr, args_len);
            let cwd_arg = read_guest_string(&mut caller, cwd_ptr, cwd_len);
            let root = caller.data().cwd();
            let reply = host_git_reply(&root, &args, &cwd_arg);
            write_guest_json(&mut caller, reply)
        },
    )?;

    linker.func_wrap(
        "env",
        "browser_cdp",
        |mut caller: Caller<'_, HostState>, body_ptr: u32, body_len: u32| -> u64 {
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd = caller.data().cwd();
            let reply = crate::browser::browser_cdp(&cwd, &body);
            write_guest_json(&mut caller, reply)
        },
    )?;

    linker.func_wrap(
        "env",
        "browser_ops",
        |mut caller: Caller<'_, HostState>, op_ptr: u32, op_len: u32, body_ptr: u32, body_len: u32| -> u64 {
            let op = read_guest_string(&mut caller, op_ptr, op_len);
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd = caller.data().cwd();
            let reply = crate::browser_ops::reply(&cwd, &op, &body);
            write_guest_json(&mut caller, reply)
        },
    )?;

    linker.func_wrap(
        "env",
        "browser_events",
        |mut caller: Caller<'_, HostState>, body_ptr: u32, body_len: u32| -> u64 {
            let body = read_guest_string(&mut caller, body_ptr, body_len);
            let cwd = caller.data().cwd();
            let reply = crate::component::browser_events_host::reply(&cwd, &body);
            write_guest_json(&mut caller, reply)
        },
    )?;

    Ok(())
}

fn kv_namespace_dir(cwd: &std::path::Path, ns: &str) -> PathBuf {
    cwd.join(".agentplug-kv").join(safe_name(ns))
}

pub(crate) fn kv_file_path(cwd: &std::path::Path, ns: &str, key: &str) -> PathBuf {
    kv_namespace_dir(cwd, ns).join(format!("{}.json", safe_name(key)))
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static CREDENTIAL_CONFIG_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn scopes_the_github_cli_store_and_helper_to_git_children() {
        let _guard = CREDENTIAL_CONFIG_LOCK.lock().unwrap();
        let credential_dir = std::env::temp_dir().join("agentplug-gh-config");
        set_github_cli_config_dir(Some(credential_dir.clone()));
        let mut command = std::process::Command::new("git");
        configure_github_git_credentials(&mut command, &["push".to_string()]);
        let envs: HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GH_CONFIG_DIR"))
                .and_then(|value| *value),
            Some(credential_dir.as_os_str())
        );
        let helper_present = envs.iter().any(|(key, value)| {
            key.to_string_lossy().starts_with("GIT_CONFIG_KEY_")
                && value.is_some_and(|value| {
                    value == std::ffi::OsStr::new("credential.https://github.com.helper")
                })
        });
        assert!(helper_present);
        set_github_cli_config_dir(None);
    }
}
