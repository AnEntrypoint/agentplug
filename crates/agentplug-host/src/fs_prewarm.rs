use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

pub const ACTION: &str = "fs_prewarm";

const MAX_PATHS_PER_CALL: usize = 4096;

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

const READER_THREADS: usize = 16;

fn read_and_discard(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(md) if md.is_file() && md.len() <= MAX_FILE_BYTES => std::fs::read(path).is_ok(),
        _ => false,
    }
}

pub fn run(params: &Value, sandbox: impl Fn(&str) -> Option<PathBuf>) -> Value {
    let Some(requested) = params.get("paths").and_then(|v| v.as_array()) else {
        return json!({"ok": false, "error": "fs_prewarm needs a \"paths\" array"});
    };
    if requested.len() > MAX_PATHS_PER_CALL {
        return json!({"ok": false, "error": format!("fs_prewarm accepts at most {MAX_PATHS_PER_CALL} paths per call, got {}", requested.len())});
    }
    let admitted: Vec<PathBuf> = requested
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(&sandbox)
        .collect();
    let refused = requested.len() - admitted.len();
    let next = AtomicUsize::new(0);
    let warmed = AtomicUsize::new(0);
    let readers = READER_THREADS.min(admitted.len()).max(1);
    std::thread::scope(|scope| {
        for _ in 0..readers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = admitted.get(i) else { break };
                if read_and_discard(path) {
                    warmed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    json!({"ok": true, "requested": requested.len(), "warmed": warmed.load(Ordering::Relaxed), "refused": refused})
}
