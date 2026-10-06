use std::fs;
use std::io::Write;
use std::path::Path;

pub const WATCHER_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const WATCHER_LOG_BACKUPS: u32 = 2;

fn backup_path(path: &Path, generation: u32) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "watcher.log".to_string());
    name.push_str(&format!(".{generation}"));
    path.with_file_name(name)
}

// The prose promises rotation at 10MB and nothing ever did it, so one busy
// project's log grew without bound (18MB measured on C:\dev\gm) and the spool
// sweep kept stat-ing a file it never trimmed. Rotate before the append, oldest
// generation first, and never let a rotation failure swallow the line.
fn rotate_if_oversized(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() <= WATCHER_LOG_MAX_BYTES {
        return;
    }
    let oldest = backup_path(path, WATCHER_LOG_BACKUPS);
    if oldest.exists() {
        let _ = fs::remove_file(&oldest);
    }
    for generation in (1..WATCHER_LOG_BACKUPS).rev() {
        let from = backup_path(path, generation);
        if from.exists() {
            let _ = fs::rename(&from, backup_path(path, generation + 1));
        }
    }
    let _ = fs::rename(path, backup_path(path, 1));
}

pub fn watcher_log_path(root: &Path) -> std::path::PathBuf {
    root.join(".gm").join("exec-spool").join(".watcher.log")
}

pub fn append_watcher_line(root: &Path, line: &str) {
    let log_path = watcher_log_path(root);
    let Some(parent) = log_path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    rotate_if_oversized(&log_path);
    let mut file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => file,
        Err(_) => return,
    };
    let _ = writeln!(file, "{line}");
}

pub fn append_watcher_event(root: &Path, event: &str) {
    append_watcher_line(root, &format!("evt: {event}"));
}
