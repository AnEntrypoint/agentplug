use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const SUCCESS_TTL: Duration = Duration::from_secs(60);
const NEGATIVE_TTL: Duration = Duration::from_secs(10);
const MAX_TOKEN_BYTES: usize = 8192;

#[derive(Clone, PartialEq, Eq)]
struct LookupKey {
    config_dir: Option<OsString>,
    path: Option<OsString>,
    relative_path_base: Option<PathBuf>,
}

impl LookupKey {
    fn current() -> Self {
        let path = std::env::var_os("PATH");
        let relative_path_base = path
            .as_deref()
            .is_some_and(|path| std::env::split_paths(path).any(|part| part.is_relative()))
            .then(|| std::env::current_dir().ok())
            .flatten();
        Self {
            config_dir: std::env::var_os("GH_CONFIG_DIR")
                .or_else(|| crate::github_cli_config_dir().map(|path| path.into_os_string())),
            path,
            relative_path_base,
        }
    }
}

#[derive(Default)]
struct Cache {
    key: Option<LookupKey>,
    token: Option<String>,
    checked_at: Option<Instant>,
    refreshing: bool,
    generation: u64,
}

struct SharedCache {
    state: Mutex<Cache>,
    changed: Condvar,
}

fn shared_cache() -> &'static SharedCache {
    static CACHE: OnceLock<SharedCache> = OnceLock::new();
    CACHE.get_or_init(|| SharedCache {
        state: Mutex::new(Cache::default()),
        changed: Condvar::new(),
    })
}

struct RefreshGuard(&'static SharedCache);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refreshing = false;
        self.0.changed.notify_all();
    }
}

pub fn github_cli_token() -> Option<String> {
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    let key = LookupKey::current();
    let shared = shared_cache();
    loop {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let ttl = if state.token.is_some() {
            SUCCESS_TTL
        } else {
            NEGATIVE_TTL
        };
        if state.key.as_ref() == Some(&key) && state.checked_at.is_some_and(|at| at.elapsed() < ttl)
        {
            return state.token.clone();
        }
        if !state.refreshing {
            if Instant::now() >= deadline {
                return None;
            }
            state.refreshing = true;
            let generation = state.generation;
            drop(state);
            let _refresh = RefreshGuard(shared);
            let token = read_github_cli_token(&key, deadline);
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.generation != generation {
                return None;
            }
            state.key = Some(key);
            state.token = token.clone();
            state.checked_at = Some(Instant::now());
            return token;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let (next, _) = shared
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|error| error.into_inner());
        drop(next);
    }
}

pub fn invalidate_github_cli_token() {
    let mut state = shared_cache()
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.token = None;
    state.checked_at = None;
    state.generation = state.generation.wrapping_add(1);
}

struct OwnedLookup {
    child: Child,
    containment: crate::process_tree::ProcessContainment,
}

impl Drop for OwnedLookup {
    fn drop(&mut self) {
        crate::process_tree::terminate_containment(&self.containment, self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn capture_secret_stdout(command: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    crate::windowless::apply_windowless(command);
    let mut child = command.spawn().ok()?;
    let containment = match crate::process_tree::establish_containment(&child) {
        Ok(containment) => containment,
        Err(_) => {
            crate::process_tree::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let mut owned = OwnedLookup { child, containment };
    let stdout = owned.child.stdout.take()?;
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("github-credential-read".to_string())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((MAX_TOKEN_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .ok()
                .map(|_| bytes);
            let _ = send.send(result);
        })
        .ok()?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    let status = owned.child.wait_timeout(remaining).ok()??;
    if !status.success() {
        return None;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let bytes = receive.recv_timeout(remaining).ok()??;
    (bytes.len() <= MAX_TOKEN_BYTES).then_some(bytes)
}

fn read_github_cli_token(key: &LookupKey, deadline: Instant) -> Option<String> {
    let mut command = Command::new("gh");
    command
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN");
    if let Some(directory) = &key.config_dir {
        command.env("GH_CONFIG_DIR", directory);
    }
    if let Some(path) = &key.path {
        command.env("PATH", path);
    }
    let bytes = capture_secret_stdout(&mut command, deadline)?;
    let token = String::from_utf8(bytes).ok()?.trim().to_string();
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic())).then_some(token)
}
