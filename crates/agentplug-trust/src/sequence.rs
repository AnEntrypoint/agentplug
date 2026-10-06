use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SEQUENCE_FILE_NAME: &str = "update-sequences.json";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub sequence: u64,
    pub sha256: String,
}

fn path(dir: &Path) -> PathBuf {
    dir.join(SEQUENCE_FILE_NAME)
}

fn read_all(dir: &Path) -> BTreeMap<String, Accepted> {
    std::fs::read_to_string(path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn accepted(dir: &Path, artifact: &str) -> Option<Accepted> {
    read_all(dir).remove(artifact)
}

pub fn record(dir: &Path, artifact: &str, sequence: u64, sha256: &str) -> std::io::Result<()> {
    let mut all = read_all(dir);
    if all
        .get(artifact)
        .map(|a| a.sequence > sequence)
        .unwrap_or(false)
    {
        return Ok(());
    }
    all.insert(
        artifact.to_string(),
        Accepted {
            sequence,
            sha256: sha256.to_ascii_lowercase(),
        },
    );
    std::fs::create_dir_all(dir)?;
    let target = path(dir);
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&all).unwrap_or_default())?;
    std::fs::rename(&tmp, &target)
}
