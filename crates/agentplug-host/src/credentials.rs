use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

const GITHUB_TOKEN_ENV_KEYS_READ_IN_PRIORITY_ORDER: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];
pub const GITHUB_TOKEN_ENV_KEYS_STRIPPED_FOR_CREDENTIAL_HELPER_RESOLUTION: [&str; 4] =
    ["GITHUB_TOKEN", "GH_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_ENTERPRISE_TOKEN"];
const GH_AUTH_TOKEN_TIMEOUT_MS: u64 = 5_000;
const GIT_AUTH_REJECTION_MARKERS: [&str; 4] = [
    "Invalid username or token",
    "Authentication failed",
    "Bad credentials",
    "Password authentication is not supported",
];
pub const GIT_CREDENTIAL_REJECTED_HINT: &str = "credential helper returned a rejected token; run gh auth status";
pub const GIT_CREDENTIAL_REJECTED_ERROR_CODE: &str = "git_credential_rejected";

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

pub fn process_env_carries_github_token() -> bool {
    GITHUB_TOKEN_ENV_KEYS_STRIPPED_FOR_CREDENTIAL_HELPER_RESOLUTION.iter().any(|k| nonempty_env(k).is_some())
}

fn gh_auth_token_resolved_fresh() -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    for key in GITHUB_TOKEN_ENV_KEYS_STRIPPED_FOR_CREDENTIAL_HELPER_RESOLUTION {
        cmd.env_remove(key);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    let status = match child.wait_timeout(Duration::from_millis(GH_AUTH_TOKEN_TIMEOUT_MS)) {
        Ok(Some(status)) => status,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut stdout).ok()?;
    Some(stdout.trim().to_string()).filter(|t| !t.is_empty())
}

pub fn resolve_github_token() -> Option<String> {
    GITHUB_TOKEN_ENV_KEYS_READ_IN_PRIORITY_ORDER
        .iter()
        .find_map(|k| nonempty_env(k))
        .or_else(gh_auth_token_resolved_fresh)
}

pub fn is_github_token_env_key(key: &str) -> bool {
    GITHUB_TOKEN_ENV_KEYS_READ_IN_PRIORITY_ORDER.contains(&key)
}

pub fn stderr_is_git_auth_rejection(stderr: &str) -> bool {
    GIT_AUTH_REJECTION_MARKERS.iter().any(|m| stderr.contains(m))
}
