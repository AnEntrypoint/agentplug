//! Records the pid of every child a dispatch spawns, so a leaked process is attributed to the
//! dispatch that made it instead of to a manual census. Attribution is also the reaper's safety
//! predicate: a pid no ledger claims is never a reaping candidate, which is what keeps the reaper
//! from becoming a bulk process filter. One JSON line per child, appended, so the host and the
//! gm-mcp bundle can both write it without locking.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LEDGER_BYTES: u64 = 1_048_576;

fn ledger_path(cwd: &Path) -> std::path::PathBuf {
    cwd.join(".gm").join("exec-spool").join(".child-pids.jsonl")
}

fn json_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "/")
        .replace('"', "")
        .replace('\n', " ")
        .replace('\r', " ");
    format!("\"{}\"", escaped)
}

pub fn record(cwd: &Path, kind: &str, verb: &str, session_id: Option<&str>, pid: u32) {
    let path = ledger_path(cwd);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let session = match session_id {
        Some(value) => json_string(value),
        None => "null".to_string(),
    };
    let line = format!(
        "{{\"ts\":{},\"writer\":\"host\",\"pid\":{},\"ppid\":{},\"kind\":\"{}\",\"verb\":{},\"session_id\":{},\"dispatch_id\":null,\"task\":null,\"port\":null,\"root\":{}}}\n",
        now_ms,
        pid,
        std::process::id(),
        kind,
        json_string(verb),
        session,
        json_string(&cwd.to_string_lossy())
    );
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LEDGER_BYTES {
            let rotated = path.with_extension("jsonl.1");
            let _ = std::fs::rename(&path, rotated);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}
