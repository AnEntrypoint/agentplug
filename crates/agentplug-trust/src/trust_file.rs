use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::hexutil;

pub const TRUST_FILE_NAME: &str = "trusted-keys.json";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Warn,
    Enforce,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Warn => "warn",
            Mode::Enforce => "enforce",
        }
    }

    fn parse(text: &str) -> Option<Mode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Mode::Off),
            "warn" => Some(Mode::Warn),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct RawKey {
    id: String,
    public_key: String,
}

#[derive(Deserialize, Default)]
struct RawTrust {
    mode: Option<String>,
    threshold: Option<u32>,
    #[serde(default)]
    keys: Vec<RawKey>,
    #[serde(default)]
    min_sequence: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
pub struct TrustedKey {
    pub id: String,
    pub public: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct Trust {
    pub path: PathBuf,
    pub configured: bool,
    pub mode: Mode,
    pub threshold: usize,
    pub keys: Vec<TrustedKey>,
    pub min_sequence: BTreeMap<String, u64>,
    pub problem: Option<String>,
}

impl Trust {
    fn unconfigured(path: PathBuf) -> Trust {
        Trust {
            path,
            configured: false,
            mode: Mode::Warn,
            threshold: 1,
            keys: Vec::new(),
            min_sequence: BTreeMap::new(),
            problem: None,
        }
    }

    fn broken(path: PathBuf, problem: String) -> Trust {
        Trust {
            path,
            configured: true,
            mode: Mode::Enforce,
            threshold: 1,
            keys: Vec::new(),
            min_sequence: BTreeMap::new(),
            problem: Some(problem),
        }
    }

    pub fn key(&self, id: &str) -> Option<&TrustedKey> {
        self.keys.iter().find(|k| k.id == id)
    }
}

pub fn trust_path(dir: &Path) -> PathBuf {
    dir.join(TRUST_FILE_NAME)
}

#[cfg(unix)]
fn writable_beyond_owner(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o022 != 0).then(|| format!("{} is group/world-writable (mode {:o}); a trust file anyone can edit pins nothing -- chmod 644 it as an administrator", path.display(), mode & 0o777))
}

#[cfg(not(unix))]
fn writable_beyond_owner(_path: &Path) -> Option<String> {
    None
}

pub fn load(dir: &Path) -> Trust {
    let path = trust_path(dir);
    let raw_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Trust::unconfigured(path),
        Err(e) => {
            return Trust::broken(
                path.clone(),
                format!("{} exists but cannot be read: {e}", path.display()),
            )
        }
    };
    if let Some(problem) = writable_beyond_owner(&path) {
        return Trust::broken(path, problem);
    }
    let raw: RawTrust = match serde_json::from_str(raw_text.trim_start_matches('\u{feff}')) {
        Ok(raw) => raw,
        Err(e) => {
            return Trust::broken(
                path.clone(),
                format!("{} does not parse: {e}", path.display()),
            )
        }
    };
    let mode = match raw.mode.as_deref() {
        None => Mode::Enforce,
        Some(text) => match Mode::parse(text) {
            Some(mode) => mode,
            None => {
                return Trust::broken(
                    path.clone(),
                    format!(
                        "{} has unknown mode {text:?}; expected off, warn or enforce",
                        path.display()
                    ),
                )
            }
        },
    };
    let mut keys = Vec::new();
    let mut ids = BTreeSet::new();
    let mut publics = BTreeSet::new();
    for raw_key in &raw.keys {
        if !crate::signature::is_key_id(&raw_key.id) {
            return Trust::broken(
                    path.clone(),
                    format!(
                        "{} key id {:?} must be 1-128 ASCII letters, digits, dots, underscores, or hyphens",
                        path.display(),
                        raw_key.id
                    ),
                );
        }
        let Some(public) = hexutil::decode_fixed::<32>(&raw_key.public_key) else {
            return Trust::broken(
                path.clone(),
                format!(
                    "{} key {:?} is not a 64-hex-digit ed25519 public key",
                    path.display(),
                    raw_key.id
                ),
            );
        };
        if !ids.insert(raw_key.id.clone()) {
            return Trust::broken(
                path.clone(),
                format!("{} lists key id {:?} twice", path.display(), raw_key.id),
            );
        }
        if !publics.insert(public) {
            return Trust::broken(
                path.clone(),
                format!(
                    "{} lists the same ed25519 public key more than once",
                    path.display()
                ),
            );
        }
        keys.push(TrustedKey {
            id: raw_key.id.clone(),
            public,
        });
    }
    let threshold = raw.threshold.unwrap_or(1) as usize;
    if mode != Mode::Off && (threshold == 0 || threshold > keys.len()) {
        return Trust::broken(
            path.clone(),
            format!(
                "{} threshold {threshold} cannot be met by {} distinct key(s)",
                path.display(),
                keys.len()
            ),
        );
    }
    Trust {
        path,
        configured: true,
        mode,
        threshold,
        keys,
        min_sequence: raw.min_sequence,
        problem: None,
    }
}
