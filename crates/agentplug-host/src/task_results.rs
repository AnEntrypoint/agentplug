use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const RETENTION_MS: u64 = 30 * 60 * 1000;
pub(crate) const TAIL_BYTES: usize = 65536;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_RECORDS: usize = 512;
const MAX_SCAN_ENTRIES: usize = 1024;

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[derive(Serialize, Deserialize, PartialEq)]
pub(crate) struct CompletedTask {
    pub schema: u32,
    pub id: String,
    pub lang: String,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_omitted_bytes: u64,
    pub stderr_omitted_bytes: u64,
}

fn store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn directory() -> PathBuf {
    crate::install_dir().join("task-results")
}

fn valid_id(id: &str) -> bool {
    id.starts_with("task-")
        && id.len() > 5
        && id.len() <= 128
        && id[5..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn checked_directory(create: bool) -> Result<Option<PathBuf>, String> {
    let path = directory();
    if create {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| error.to_string())?;
    }
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_dir() {
        return Err("task result directory must be a real directory".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("task result directory must be private to its owner".to_string());
        }
    }
    Ok(Some(path))
}

fn read_record(path: &Path, id: &str) -> Result<Option<CompletedTask>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err("task result is not a bounded regular file".to_string());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if metadata.uid() != effective_uid() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("task result file must be private to the effective user".to_string());
        }
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("opened task result is not a regular file".to_string());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err("task result exceeded its read bound".to_string());
    }
    let record: CompletedTask =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    validate_record(&record, id)?;
    Ok(Some(record))
}

fn validate_record(record: &CompletedTask, id: &str) -> Result<(), String> {
    if record.schema != 1
        || record.id != id
        || !valid_id(&record.id)
        || record.finished_ms < record.started_ms
        || record.stdout.len() > TAIL_BYTES
        || record.stderr.len() > TAIL_BYTES
        || record.stdout_omitted_bytes > u64::MAX - TAIL_BYTES as u64
        || record.stderr_omitted_bytes > u64::MAX - TAIL_BYTES as u64
    {
        return Err(
            "task result schema, identity, timestamps or tail bound is invalid".to_string(),
        );
    }
    Ok(())
}

fn expired(record: &CompletedTask, now_ms: u64) -> bool {
    now_ms.saturating_sub(record.finished_ms) >= RETENTION_MS
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("json.pending.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().expect("task result parent"))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}

pub(crate) fn save(records: Vec<CompletedTask>, now_ms: u64) -> Result<(), String> {
    let _lock = store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(directory) = checked_directory(!records.is_empty())? else {
        return Ok(());
    };
    let mut sizes = std::collections::HashMap::new();
    let mut total_bytes = 0u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for (scanned, entry) in std::fs::read_dir(&directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if scanned >= MAX_SCAN_ENTRIES || std::time::Instant::now() >= deadline {
            return Err("task result directory scan exceeded its bound".to_string());
        }
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let id = name.strip_suffix(".json").filter(|id| valid_id(id));
        let metadata =
            std::fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if let Some(id) = id {
            if let Some(record) = read_record(&entry.path(), id)? {
                if expired(&record, now_ms) {
                    std::fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
                    continue;
                }
            }
        }
        if sizes.len() >= MAX_RECORDS {
            return Err("task result store exceeds its record bound".to_string());
        }
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or("task result byte count overflow")?;
        sizes.insert(name, metadata.len());
        if total_bytes > MAX_STORE_BYTES {
            return Err("task result store exceeds its byte bound".to_string());
        }
    }
    let mut writes = Vec::new();
    for record in records {
        validate_record(&record, &record.id)?;
        let name = format!("{}.json", record.id);
        let path = directory.join(&name);
        if let Some(existing) = read_record(&path, &record.id)? {
            if existing != record {
                return Err("an immutable task result already has different content".to_string());
            }
            continue;
        }
        let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("task result serialization exceeds its bound".to_string());
        }
        total_bytes = total_bytes
            .checked_add(bytes.len() as u64)
            .ok_or("task result byte count overflow")?;
        sizes.insert(name, bytes.len() as u64);
        if sizes.len() > MAX_RECORDS || total_bytes > MAX_STORE_BYTES {
            return Err(
                "task result store is full; unexpired results were not evicted".to_string(),
            );
        }
        writes.push((path, bytes));
    }
    for (path, bytes) in writes {
        atomic_write(&path, &bytes)?;
    }
    Ok(())
}

pub(crate) fn output(id: &str, max_bytes: usize, now_ms: u64) -> Result<Option<Value>, String> {
    if !valid_id(id) {
        return Err("task ID is not a safe result filename".to_string());
    }
    let _lock = store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(directory) = checked_directory(false)? else {
        return Ok(None);
    };
    let path = directory.join(format!("{id}.json"));
    let Some(record) = read_record(&path, id)? else {
        return Ok(None);
    };
    if expired(&record, now_ms) {
        std::fs::remove_file(&path).map_err(|error| error.to_string())?;
        return Ok(None);
    }
    let tail = |bytes: &[u8]| {
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(max_bytes)..]).into_owned()
    };
    Ok(Some(json!({
        "ok": true, "id": id, "running": false, "exit_code": record.exit_code,
        "stdout": tail(&record.stdout), "stderr": tail(&record.stderr),
        "stdout_omitted_bytes": record.stdout_omitted_bytes + record.stdout.len().saturating_sub(max_bytes) as u64,
        "stderr_omitted_bytes": record.stderr_omitted_bytes + record.stderr.len().saturating_sub(max_bytes) as u64,
        "retained_after_handoff": true,
        "retention_expires_ms": record.finished_ms.saturating_add(RETENTION_MS),
    })))
}

pub(crate) fn remove(id: &str) -> Result<bool, String> {
    if !valid_id(id) {
        return Err("task ID is not a safe result filename".to_string());
    }
    let _lock = store_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(directory) = checked_directory(false)? else {
        return Ok(false);
    };
    let path = directory.join(format!("{id}.json"));
    if read_record(&path, id)?.is_none() {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok(true)
}
