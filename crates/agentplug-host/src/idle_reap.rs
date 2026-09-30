use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum IdleReap {
    After(Duration),
    Never,
}

impl IdleReap {
    pub(crate) fn parse_launch_token(token: &str) -> Option<Result<IdleReap, String>> {
        if token == "keep_alive" {
            return Some(Ok(IdleReap::Never));
        }
        let value = token.strip_prefix("idle_timeout_ms=")?;
        Some(match value.parse::<u64>() {
            Ok(0) => Ok(IdleReap::Never),
            Ok(ms) => Ok(IdleReap::After(Duration::from_millis(ms))),
            Err(_) => Err(format!("idle_timeout_ms={value} is not a whole number of milliseconds (0 or keep_alive means never idle-reaped)")),
        })
    }

    fn encode(self) -> String {
        match self {
            IdleReap::Never => "never".to_string(),
            IdleReap::After(d) => d.as_millis().to_string(),
        }
    }

    fn decode(raw: &str) -> Option<IdleReap> {
        match raw.trim() {
            "never" => Some(IdleReap::Never),
            ms => ms.parse::<u64>().ok().filter(|ms| *ms > 0).map(|ms| IdleReap::After(Duration::from_millis(ms))),
        }
    }

    pub(crate) fn report(reap: Option<IdleReap>) -> Value {
        match reap {
            None => json!("default"),
            Some(IdleReap::Never) => json!("never"),
            Some(IdleReap::After(d)) => json!(d.as_millis() as u64),
        }
    }
}

fn sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("idle-reap.txt")
}

pub(crate) fn record(profile_dir: &Path, reap: Option<IdleReap>) {
    let path = sidecar_path(profile_dir);
    match reap {
        Some(reap) => {
            let _ = std::fs::create_dir_all(profile_dir);
            let _ = std::fs::write(&path, reap.encode());
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub(crate) fn recorded(profile_dir: &Path) -> Option<IdleReap> {
    std::fs::read_to_string(sidecar_path(profile_dir)).ok().and_then(|raw| IdleReap::decode(&raw))
}
