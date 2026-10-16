use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentplug_host::install_dir;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_download_tmp_name(name: &str) -> bool {
    let Some(suffix) = name.rsplit_once(".tmp.").map(|(_, s)| s) else {
        return false;
    };
    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
}

fn gc_stale_tmp_files_in(dir: &Path, min_age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !is_download_tmp_name(name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < min_age {
            continue;
        }
        if let Err(e) = fs::remove_file(&path) {
            eprintln!(
                "[agentplug] failed to gc stale tmp file {}: {e}",
                path.display()
            );
        } else {
            eprintln!(
                "[agentplug] gc'd stale tmp file {} (age {}s)",
                path.display(),
                age.as_secs()
            );
        }
    }
}

pub fn gc_stale_tmp_files(min_age: Duration) {
    gc_stale_tmp_files_in(&install_dir().join("plugins"), min_age);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            gc_stale_tmp_files_in(exe_dir, min_age);
        }
    }
    enforce_install_dir_backup_retention(true);
    enforce_wasi_fs_retention(true);
    enforce_plugin_cache_retention(true);
}

pub const INSTALL_BACKUP_RETAIN: usize = 2;

pub const INSTALL_BACKUP_BYTES_CEILING: u64 = 64 * 1024 * 1024;

const INSTALL_ASSET_EXTS: [&str; 3] = ["exe", "wasm", "mjs"];

#[derive(Debug, Default, Clone)]
pub struct BackupRetentionPlan {
    pub stem: String,
    pub keep: Vec<PathBuf>,
    pub delete: Vec<PathBuf>,
    pub keep_bytes: u64,
    pub delete_bytes: u64,
}

fn file_extension_lower(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
}

fn install_backup_is_live(rest: &str) -> bool {
    let rest = rest.to_ascii_lowercase();
    matches!(
        rest.as_str(),
        "version" | "sha256" | "no-self-update" | "checked" | "prev" | "new" | "new2"
    ) || rest.ends_with(".json")
        || rest.starts_with("replace.")
        || rest.starts_with("pinned")
}

fn install_backup_sibling_rest(name: &str, stem: &str) -> Option<String> {
    if name == stem {
        return None;
    }
    let (stem_base, stem_ext) = stem.rsplit_once('.')?;
    if let Some(rest) = name.strip_prefix(&format!("{stem}.")) {
        return Some(rest.to_string());
    }
    let (name_base, name_ext) = name.rsplit_once('.')?;
    if !name_ext.eq_ignore_ascii_case(stem_ext) {
        return None;
    }
    name_base
        .strip_prefix(&format!("{stem_base}."))
        .map(str::to_string)
}

fn same_existing_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn install_asset_stems(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut groups: std::collections::BTreeMap<(String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !entry.metadata().map(|meta| meta.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(extension) = file_extension_lower(&name) else {
            continue;
        };
        if !INSTALL_ASSET_EXTS.contains(&extension.as_str()) {
            continue;
        }
        let group = name.split('.').next().unwrap_or_default().to_ascii_lowercase();
        groups.entry((group, extension)).or_default().push(name);
    }
    groups
        .into_values()
        .filter_map(|mut names| {
            names.sort_by_key(|name| (name.matches('.').count(), name.len(), name.clone()));
            names.into_iter().next()
        })
        .collect()
}

fn modified_ms_of(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|age| age.as_millis() as u64)
        .unwrap_or(0)
}

pub fn plan_install_backup_retention(
    dir: &Path,
    stem: &str,
    pinned: &[PathBuf],
) -> BackupRetentionPlan {
    let mut plan = BackupRetentionPlan {
        stem: stem.to_string(),
        ..Default::default()
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return plan;
    };
    let mut rows: Vec<(PathBuf, u64, u64)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(rest) = install_backup_sibling_rest(&name, stem) else {
            continue;
        };
        if install_backup_is_live(&rest) {
            continue;
        }
        let path = entry.path();
        if pinned.iter().any(|pin| same_existing_path(pin, &path)) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        rows.push((path, meta.len(), modified_ms_of(&meta)));
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    for (index, (path, bytes, _)) in rows.into_iter().enumerate() {
        let over_count = index >= INSTALL_BACKUP_RETAIN;
        let over_bytes = !plan.keep.is_empty()
            && plan.keep_bytes.saturating_add(bytes) > INSTALL_BACKUP_BYTES_CEILING;
        if over_count || over_bytes {
            plan.delete.push(path);
            plan.delete_bytes = plan.delete_bytes.saturating_add(bytes);
        } else {
            plan.keep_bytes = plan.keep_bytes.saturating_add(bytes);
            plan.keep.push(path);
        }
    }
    plan
}

pub fn apply_install_backup_retention(
    dir: &Path,
    stem: &str,
    pinned: &[PathBuf],
    apply: bool,
) -> BackupRetentionPlan {
    let plan = plan_install_backup_retention(dir, stem, pinned);
    for path in &plan.delete {
        if !apply {
            continue;
        }
        match fs::remove_file(path) {
            Ok(()) => eprintln!(
                "[agentplug] install-dir backup retention: removed {}",
                path.display()
            ),
            Err(error) => eprintln!(
                "[agentplug] install-dir backup retention: could not remove {}: {error}",
                path.display()
            ),
        }
    }
    plan
}

pub fn install_dir_backup_pins() -> Vec<PathBuf> {
    let mut pinned = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        pinned.push(exe);
    }
    if let Some(canonical) = canonical_runner_exe() {
        pinned.push(canonical);
    }
    pinned
}

pub fn install_dir_roots() -> Vec<PathBuf> {
    let mut roots = vec![install_dir()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let dir = dir.to_path_buf();
            if !roots.iter().any(|root| same_existing_path(root, &dir)) {
                roots.push(dir);
            }
        }
    }
    roots
}

fn install_dir_retention_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in install_dir_roots() {
        for dir in [root.clone(), root.join("plugins")] {
            if !dirs.iter().any(|seen| same_existing_path(seen, &dir)) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

pub fn enforce_install_dir_backup_retention(apply: bool) -> Vec<BackupRetentionPlan> {
    let pinned = install_dir_backup_pins();
    let mut plans = Vec::new();
    for dir in install_dir_retention_dirs() {
        for stem in install_asset_stems(&dir) {
            plans.push(apply_install_backup_retention(&dir, &stem, &pinned, apply));
        }
    }
    let kept_count: usize = plans.iter().map(|plan| plan.keep.len()).sum();
    let deleted_count: usize = plans.iter().map(|plan| plan.delete.len()).sum();
    let kept: u64 = plans.iter().map(|plan| plan.keep_bytes).sum();
    let reclaimed: u64 = plans.iter().map(|plan| plan.delete_bytes).sum();
    if deleted_count > 0 || !apply {
        for plan in &plans {
            if plan.delete.is_empty() {
                continue;
            }
            eprintln!(
                "[agentplug] install-dir backup retention{}: {} {} of {} ({} bytes)",
                if apply { "" } else { " (dry run)" },
                if apply { "removed" } else { "would remove" },
                plan.delete.len(),
                plan.stem,
                plan.delete_bytes,
            );
        }
        eprintln!(
            "[agentplug] install-dir backup retention{}: {} {} backup(s) ({} bytes) reclaimed, {} kept ({} bytes); bound {} backup(s) and {} bytes per asset",
            if apply { "" } else { " (dry run)" },
            if apply { "removed" } else { "would remove" },
            deleted_count,
            reclaimed,
            kept_count,
            kept,
            INSTALL_BACKUP_RETAIN,
            INSTALL_BACKUP_BYTES_CEILING,
        );
    }
    plans
}

pub const WASI_FS_RETAIN_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn is_wasi_fs_instance_key(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Default, Clone)]
pub struct WasiFsRetentionPlan {
    pub delete: Vec<PathBuf>,
    pub delete_bytes: u64,
    pub live: Vec<PathBuf>,
    pub live_bytes: u64,
}

fn wasi_fs_instance_stats(dir: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut newest_ms = 0u64;
    let Ok(entries) = fs::read_dir(dir) else {
        return (bytes, newest_ms);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let (inner_bytes, inner_newest) = wasi_fs_instance_stats(&entry.path());
            bytes = bytes.saturating_add(inner_bytes);
            newest_ms = newest_ms.max(inner_newest);
            continue;
        }
        bytes = bytes.saturating_add(meta.len());
        newest_ms = newest_ms.max(modified_ms_of(&meta));
    }
    (bytes, newest_ms)
}

pub fn plan_wasi_fs_retention(dir: &Path, min_age: Duration) -> WasiFsRetentionPlan {
    let mut plan = WasiFsRetentionPlan::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return plan;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|age| age.as_millis() as u64)
        .unwrap_or(0);
    let mut rows: Vec<(PathBuf, u64, u64)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_wasi_fs_instance_key(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let path = entry.path();
        let (bytes, newest_ms) = wasi_fs_instance_stats(&path);
        rows.push((path, bytes, newest_ms.max(modified_ms_of(&meta))));
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let min_age_ms = min_age.as_millis() as u64;
    for (path, bytes, newest_ms) in rows.into_iter() {
        if newest_ms == 0 || now_ms.saturating_sub(newest_ms) < min_age_ms {
            plan.live.push(path);
            plan.live_bytes = plan.live_bytes.saturating_add(bytes);
            continue;
        }
        plan.delete.push(path);
        plan.delete_bytes = plan.delete_bytes.saturating_add(bytes);
    }
    plan
}

pub fn apply_wasi_fs_retention(
    dir: &Path,
    min_age: Duration,
    apply: bool,
) -> WasiFsRetentionPlan {
    let plan = plan_wasi_fs_retention(dir, min_age);
    for path in &plan.delete {
        if !apply {
            continue;
        }
        match fs::remove_dir_all(path) {
            Ok(()) => eprintln!("[agentplug] wasi-fs retention: removed {}", path.display()),
            Err(error) => eprintln!(
                "[agentplug] wasi-fs retention: could not remove {}: {error} -- a live instance still holds it open, so it stays",
                path.display()
            ),
        }
    }
    plan
}

pub fn enforce_wasi_fs_retention(apply: bool) -> WasiFsRetentionPlan {
    let mut plan = WasiFsRetentionPlan::default();
    for dir in install_dir_roots() {
        let dir = dir.join("wasi-fs");
        if !dir.exists() {
            continue;
        }
        let found = apply_wasi_fs_retention(&dir, WASI_FS_RETAIN_AGE, apply);
        plan.delete.extend(found.delete);
        plan.live.extend(found.live);
        plan.delete_bytes = plan.delete_bytes.saturating_add(found.delete_bytes);
        plan.live_bytes = plan.live_bytes.saturating_add(found.live_bytes);
    }
    if !plan.delete.is_empty() || !apply {
        eprintln!(
            "[agentplug] wasi-fs retention{}: {} {} instance(s) ({} bytes) reclaimed, {} live ({} bytes) kept; bound = instances written within {}s",
            if apply { "" } else { " (dry run)" },
            if apply { "removed" } else { "would remove" },
            plan.delete.len(),
            plan.delete_bytes,
            plan.live.len(),
            plan.live_bytes,
            WASI_FS_RETAIN_AGE.as_secs(),
        );
    }
    plan
}

fn shared_plugin_cache_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".agentplug").join("plugin-cache"))
}

fn plugin_payload_cache_path(expected_sha256_hex: &str) -> Option<PathBuf> {
    shared_plugin_cache_dir()
        .map(|dir| dir.join(format!("{}.wasm", expected_sha256_hex.to_ascii_lowercase())))
}

fn plugin_payload_use_path(expected_sha256_hex: &str) -> Option<PathBuf> {
    shared_plugin_cache_dir()
        .map(|dir| dir.join(format!("{}.used", expected_sha256_hex.to_ascii_lowercase())))
}

fn shared_plugin_payload(expected_sha256_hex: &str) -> Option<PathBuf> {
    let path = plugin_payload_cache_path(expected_sha256_hex)?;
    if !path.exists() {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    sha256_hex(&bytes)
        .eq_ignore_ascii_case(expected_sha256_hex)
        .then_some(path)
}

fn mark_shared_payload_used(expected_sha256_hex: &str) {
    let Some(path) = plugin_payload_use_path(expected_sha256_hex) else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!(
            "[agentplug] plugin payload cache: could not create {}: {e}",
            dir.display()
        );
        return;
    }
    let tmp = path.with_extension(format!("used.tmp.{}", std::process::id()));
    if fs::write(&tmp, now_ms_for_marker().to_string()).is_err() {
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        eprintln!(
            "[agentplug] plugin payload cache: could not record use of {}: {e}",
            path.display()
        );
    }
}

fn link_or_copy_payload(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("wasm.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    if let Err(link_err) = fs::hard_link(src, &tmp) {
        eprintln!(
            "[agentplug] plugin payload cache: could not hard-link {} -> {} ({link_err}) -- copying instead",
            src.display(),
            dest.display()
        );
        fs::copy(src, &tmp)?;
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn restore_wasm_atomically(prev: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("wasm.restore.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::copy(prev, &tmp)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn populate_shared_plugin_payload(src: &Path, expected_sha256_hex: &str) {
    let Some(path) = plugin_payload_cache_path(expected_sha256_hex) else {
        return;
    };
    if path.exists() {
        mark_shared_payload_used(expected_sha256_hex);
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!(
            "[agentplug] plugin payload cache: could not create {}: {e}",
            dir.display()
        );
        return;
    }
    let tmp = path.with_extension(format!("wasm.stage.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    if let Err(e) = fs::hard_link(src, &tmp).or_else(|_| fs::copy(src, &tmp).map(|_| ())) {
        eprintln!(
            "[agentplug] plugin payload cache: could not stage {}: {e}",
            path.display()
        );
        return;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        eprintln!(
            "[agentplug] plugin payload cache: could not install {}: {e}",
            path.display()
        );
    }
}

pub const PLUGIN_CACHE_RETAIN_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn shared_payload_newest_ms(blob: &Path, use_marker: &Path) -> u64 {
    let mut newest = fs::metadata(blob)
        .map(|meta| modified_ms_of(&meta))
        .unwrap_or(0);
    if let Ok(meta) = fs::metadata(use_marker) {
        newest = newest.max(modified_ms_of(&meta));
    }
    newest
}

pub fn enforce_plugin_cache_retention(apply: bool) -> (usize, u64) {
    let Some(dir) = shared_plugin_cache_dir() else {
        return (0, 0);
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return (0, 0);
    };
    let now_ms = now_ms_for_marker();
    let min_age_ms = PLUGIN_CACHE_RETAIN_AGE.as_millis() as u64;
    let mut stale: Vec<(PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(sha) = name.strip_suffix(".wasm") else {
            continue;
        };
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let newest_ms = shared_payload_newest_ms(&path, &path.with_extension("used"));
        if newest_ms == 0 || now_ms.saturating_sub(newest_ms) < min_age_ms {
            continue;
        }
        stale.push((path, meta.len()));
    }
    stale.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut removed = 0usize;
    let mut reclaimed = 0u64;
    for (path, bytes) in &stale {
        if apply {
            match fs::remove_file(path) {
                Ok(()) => {
                    let _ = fs::remove_file(path.with_extension("used"));
                    removed += 1;
                    reclaimed = reclaimed.saturating_add(*bytes);
                }
                Err(error) => {
                    eprintln!(
                        "[agentplug] plugin payload cache: could not remove {}: {error}",
                        path.display()
                    );
                    continue;
                }
            }
        } else {
            removed += 1;
            reclaimed = reclaimed.saturating_add(*bytes);
        }
    }
    if removed > 0 || !apply {
        eprintln!(
            "[agentplug] plugin payload cache retention{}: {} {} entry(ies) ({} bytes) reclaimed; bound = entries handed out within {}s",
            if apply { "" } else { " (dry run)" },
            if apply { "removed" } else { "would remove" },
            removed,
            reclaimed,
            PLUGIN_CACHE_RETAIN_AGE.as_secs(),
        );
    }
    (removed, reclaimed)
}

fn release_version_from_tag(tag: &str) -> Option<String> {
    let version = tag.strip_prefix('v')?;
    is_recognized_release_semver(version).then(|| version.to_string())
}

fn extract_version_from_release_url(url: &str) -> Option<String> {
    let idx = url.find("/releases/download/")?;
    let rest = &url[idx + "/releases/download/".len()..];
    let tag = rest.split('/').next()?;
    if tag.is_empty() {
        return None;
    }
    release_version_from_tag(tag)
}

fn is_safe_identifier_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_safe_plugin_name(plugin_name: &str) -> bool {
    plugin_name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && is_safe_identifier_component(plugin_name)
}

fn is_safe_github_repo(repo: &str) -> bool {
    let mut segments = repo.split('/');
    matches!(
        (segments.next(), segments.next(), segments.next()),
        (Some(owner), Some(name), None)
            if is_safe_identifier_component(owner) && is_safe_identifier_component(name)
    )
}

fn resolve_latest_tag_via_release_page(repo: &str) -> Option<String> {
    let page_url = format!("https://github.com/{repo}/releases/latest");
    let resp = agentplug_host::shared_agent().get(&page_url).call().ok()?;
    let resolved_url = resp.get_url();
    let idx = resolved_url.find("/releases/tag/")?;
    let tag = resolved_url[idx + "/releases/tag/".len()..]
        .split('/')
        .next()?;
    if tag.is_empty() {
        return None;
    }
    release_version_from_tag(tag)
}

fn release_has_unambiguous_asset_pair(release: &serde_json::Value, asset_basename: &str) -> bool {
    let Some(assets) = release.get("assets").and_then(|assets| assets.as_array()) else {
        return false;
    };
    let wasm = format!("{asset_basename}.wasm");
    let sha256 = format!("{wasm}.sha256");
    let count = |wanted: &str| {
        assets
            .iter()
            .filter(|asset| asset.get("name").and_then(|name| name.as_str()) == Some(wanted))
            .count()
    };
    count(&wasm) == 1 && count(&sha256) == 1
}

fn release_candidate_version(
    release: &serde_json::Value,
    asset_basenames: &[String],
) -> Option<String> {
    if release.get("draft").and_then(|value| value.as_bool()) != Some(false)
        || release.get("prerelease").and_then(|value| value.as_bool()) != Some(false)
        || release
            .get("published_at")
            .and_then(|value| value.as_str())
            .is_none_or(str::is_empty)
    {
        return None;
    }
    let tag = release.get("tag_name").and_then(|value| value.as_str())?;
    let version = release_version_from_tag(tag)?;
    asset_basenames
        .iter()
        .any(|asset_basename| release_has_unambiguous_asset_pair(release, asset_basename))
        .then_some(version)
}

fn retain_newer_release_version(latest: &mut Option<String>, candidate: String) {
    if latest
        .as_deref()
        .and_then(|known| compare_release_semver(&candidate, known))
        .is_none_or(|order| order.is_gt())
    {
        *latest = Some(candidate);
    }
}

fn try_ensure_plugin_installed_via_direct_release_latest(
    spec: &PluginAssetSpec,
    dest: &Path,
    version_file: &Path,
) -> anyhow::Result<PathBuf> {
    let sha_url = format!(
        "https://github.com/{}/releases/latest/download/{}.wasm.sha256",
        spec.repo, spec.asset_basename
    );
    let sha_resp = agentplug_host::shared_agent().get(&sha_url).call()?;
    let resolved_url = sha_resp.get_url().to_string();
    let version = extract_version_from_release_url(&resolved_url)
        .or_else(|| resolve_latest_tag_via_release_page(&spec.repo))
        .ok_or_else(|| {
            anyhow::anyhow!("could not determine release tag from redirect target {resolved_url} (requested {sha_url}), and the releases/latest page fallback also failed")
        })?;
    if !is_recognized_release_semver(&version) {
        anyhow::bail!(
            "latest release tag {version:?} for {} is not X.Y.Z semver",
            spec.repo
        );
    }
    let base = format!(
        "https://github.com/{}/releases/download/v{version}",
        spec.repo
    );
    let sha_line = agentplug_host::shared_agent()
        .get(&format!("{base}/{}.wasm.sha256", spec.asset_basename))
        .call()?
        .into_string()?;
    let expected_sha = sha_line
        .split_whitespace()
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "empty sha256 sidecar for {} at {sha_url}",
                spec.asset_basename
            )
        })?
        .to_string();

    let wasm_url = format!("{base}/{}.wasm", spec.asset_basename);
    snapshot_prev_wasm_and_version(dest, version_file)?;
    if let Some(cached) = shared_plugin_payload(&expected_sha) {
        match link_or_copy_payload(&cached, dest) {
            Ok(()) => {
                mark_shared_payload_used(&expected_sha);
                eprintln!(
                    "[agentplug] plugin {} {version} served from the shared payload cache ({}), no download needed",
                    spec.asset_basename,
                    cached.display()
                );
                record_plugin_install(dest, version_file, &version)?;
                return Ok(dest.to_path_buf());
            }
            Err(e) => eprintln!(
                "[agentplug] plugin {}: could not link the cached payload ({e:#}) -- downloading it instead",
                spec.asset_basename
            ),
        }
    }
    download_and_verify(&wasm_url, dest, &expected_sha)?;
    populate_shared_plugin_payload(dest, &expected_sha);
    record_plugin_install(dest, version_file, &version)?;
    eprintln!(
        "[agentplug] {} installed via direct release-asset download {wasm_url} (api.github.com path failed or was blocked)",
        spec.asset_basename
    );
    Ok(dest.to_path_buf())
}

fn github_api_request(url: &str) -> ureq::Request {
    agentplug_host::shared_agent()
        .get(url)
        .set("User-Agent", "agentplug-runner")
}

enum GithubToken {
    Environment(String),
    Cli(String),
}

fn github_token() -> Option<GithubToken> {
    let explicit = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty());
    explicit
        .map(GithubToken::Environment)
        .or_else(|| agentplug_host::github_cli_token().map(GithubToken::Cli))
}

fn github_api_call(url: &str) -> Result<ureq::Response, ureq::Error> {
    let Some(token) = github_token() else {
        return github_api_request(url).call();
    };
    let from_cli = matches!(&token, GithubToken::Cli(_));
    let token = match token {
        GithubToken::Environment(token) | GithubToken::Cli(token) => token,
    };
    match github_api_request(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Err(ureq::Error::Status(401, _)) => {
            if from_cli {
                agentplug_host::invalidate_github_cli_token();
            }
            eprintln!("[agentplug] GitHub credentials rejected (401 Bad credentials) fetching {url} -- retrying unauthenticated");
            github_api_request(url).call()
        }
        other => other,
    }
}

fn describe_github_api_error(url: &str, err: ureq::Error) -> anyhow::Error {
    match &err {
        ureq::Error::Status(403, resp) => {
            let remaining = resp.header("x-ratelimit-remaining").unwrap_or("?");
            let reset = resp.header("x-ratelimit-reset").unwrap_or("?");
            anyhow::anyhow!(
                "GitHub API rate-limited fetching {url} (403, ratelimit-remaining={remaining}, ratelimit-reset={reset} unix secs) -- set GITHUB_TOKEN or GH_TOKEN to raise the limit from 60/hr to 5000/hr"
            )
        }
        ureq::Error::Status(code, _) => {
            anyhow::anyhow!("GitHub API returned {code} fetching {url}")
        }
        ureq::Error::Transport(_) => {
            anyhow::Error::from(err).context(format!("network error fetching {url}"))
        }
    }
}

pub fn download_and_verify(url: &str, dest: &Path, expected_sha256_hex: &str) -> anyhow::Result<()> {
    let resp = agentplug_host::download_agent().get(url).call()?;
    let mut reader = resp.into_reader();
    let mut bytes = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256_hex) {
        anyhow::bail!(
            "sha256 mismatch downloading {url}: expected {expected_sha256_hex}, got {actual}"
        );
    }
    write_download_atomically(dest, &bytes)?;
    Ok(())
}

fn write_download_atomically(dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    let mut tmp_created = false;
    let write_result = (|| -> anyhow::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        tmp_created = true;
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        if tmp_created {
            if let Err(rm_err) = fs::remove_file(&tmp) {
                if rm_err.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "[agentplug] failed to remove incomplete tmp file {}: {rm_err}",
                        tmp.display()
                    );
                }
            }
        }
        return Err(e);
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

struct PluginAssetSpec {
    repo: String,
    asset_basename: String,
}

fn gm_asset_basename() -> &'static str {
    "plugkit-slim"
}

fn builtin_plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    match plugin_name {
        "gm" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/plugkit-bin".to_string(),
            asset_basename: gm_asset_basename().to_string(),
        }),
        "bert" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-bert-bin".to_string(),
            asset_basename: "bert".to_string(),
        }),
        "libsql" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-libsql-bin".to_string(),
            asset_basename: "libsql".to_string(),
        }),
        "treesitter" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-treesitter-bin".to_string(),
            asset_basename: "treesitter".to_string(),
        }),
        "crux" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-crux-bin".to_string(),
            asset_basename: "crux".to_string(),
        }),
        "codeinsight" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-codeinsight-bin".to_string(),
            asset_basename: "codeinsight".to_string(),
        }),
        _ => None,
    }
}

#[derive(serde::Deserialize)]
struct ProjectPluginSpec {
    name: String,
    repo: String,
    asset_basename: String,
}

fn project_plugin_spec_is_safe(spec: &ProjectPluginSpec) -> bool {
    is_safe_plugin_name(&spec.name)
        && is_safe_github_repo(&spec.repo)
        && is_safe_identifier_component(&spec.asset_basename)
}

fn project_declared_plugin_specs(project_root: &Path) -> Vec<ProjectPluginSpec> {
    let path = project_root.join(".agentplug").join("plugins.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<ProjectPluginSpec>>(&raw) {
        Ok(specs) => specs
            .into_iter()
            .filter(|spec| {
                let safe = project_plugin_spec_is_safe(spec);
                if !safe {
                    eprintln!(
                        "[agentplug] {} has an unsafe plugin spec for {:?} -- ignoring it",
                        path.display(),
                        spec.name
                    );
                }
                safe
            })
            .collect(),
        Err(e) => {
            eprintln!("[agentplug] {} exists but does not parse as an array of {{name,repo,asset_basename}} -- ignoring: {e}", path.display());
            Vec::new()
        }
    }
}

fn plugin_asset_spec_for_roots(
    plugin_name: &str,
    known_roots: &[PathBuf],
) -> Option<PluginAssetSpec> {
    for root in known_roots {
        for spec in project_declared_plugin_specs(root) {
            if spec.name == plugin_name {
                return Some(PluginAssetSpec {
                    repo: spec.repo,
                    asset_basename: spec.asset_basename,
                });
            }
        }
    }
    builtin_plugin_asset_spec(plugin_name)
}

fn plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    let mut roots = crate::daemon::read_known_project_roots();
    if let Ok(cwd) = std::env::current_dir() {
        if !roots.contains(&cwd) {
            roots.push(cwd);
        }
    }
    plugin_asset_spec_for_roots(plugin_name, &roots)
}

pub fn plugin_wasm_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.wasm"))
}

fn plugin_version_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.version"))
}

fn snapshot_prev_wasm_and_version(dest: &Path, version_file: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        fs::copy(dest, dest.with_extension("wasm.prev"))?;
    }
    if version_file.exists() {
        fs::copy(version_file, version_file.with_extension("version.prev"))?;
    }
    Ok(())
}

fn restore_plugin_snapshot(dest: &Path, version_file: &Path) -> anyhow::Result<()> {
    let prev_dest = dest.with_extension("wasm.prev");
    let prev_version_file = version_file.with_extension("version.prev");
    if prev_dest.exists() {
        fs::copy(prev_dest, dest)?;
    } else if let Err(error) = fs::remove_file(dest) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    if prev_version_file.exists() {
        fs::copy(prev_version_file, version_file)?;
    } else if let Err(error) = fs::remove_file(version_file) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
}

fn record_plugin_install(dest: &Path, version_file: &Path, version: &str) -> anyhow::Result<()> {
    if let Err(write_error) = fs::write(version_file, version) {
        return match restore_plugin_snapshot(dest, version_file) {
            Ok(()) => Err(anyhow::anyhow!(
                "could not record installed plugin version {version}: {write_error}; restored the prior plugin snapshot"
            )),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "could not record installed plugin version {version}: {write_error}; restoring the prior plugin snapshot also failed: {rollback_error}"
            )),
        };
    }
    Ok(())
}

fn plugin_build_record_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.build.json"))
}

fn write_plugin_build_record(plugin_name: &str, repo: &str, version: &str, dest: &Path) {
    let bytes = match fs::read(dest) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!(
                "[agentplug daemon] plugin {plugin_name} build record skipped: cannot read {}: {error}",
                dest.display()
            );
            return;
        }
    };
    let record = serde_json::json!({
        "plugin": plugin_name,
        "version": version,
        "source_sha": release_source_head(repo, version),
        "wasm_sha256": sha256_hex(&bytes),
        "wasm_bytes": bytes.len(),
        "installed_at_ms": now_ms_for_marker(),
        "origin": "release",
    });
    if let Err(error) = fs::write(plugin_build_record_path(plugin_name), record.to_string()) {
        eprintln!("[agentplug daemon] plugin {plugin_name} build record not written: {error}");
    }
}

fn release_source_head(repo: &str, version: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/v{version}");
    let text = github_api_call(&url).ok()?.into_string().ok()?;
    let release: serde_json::Value = serde_json::from_str(&text).ok()?;
    release
        .get("body")?
        .as_str()?
        .lines()
        .find_map(|line| line.trim().strip_prefix("source-head:"))
        .map(|sha| sha.trim().to_ascii_lowercase())
        .filter(|sha| (7..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
}

const RUNNER_BIN_REPO: &str = "AnEntrypoint/agentplug-bin";

fn runner_asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("agentplug-runner-windows-x64.exe"),
        ("windows", "aarch64") => Some("agentplug-runner-windows-arm64.exe"),
        ("macos", "x86_64") => Some("agentplug-runner-macos-x64"),
        ("macos", "aarch64") => Some("agentplug-runner-macos-arm64"),
        ("linux", "x86_64") => Some("agentplug-runner-linux-x64"),
        ("linux", "aarch64") => Some("agentplug-runner-linux-arm64"),
        _ => None,
    }
}

pub(crate) fn runner_asset_name() -> Option<&'static str> {
    runner_asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn runner_version_path() -> PathBuf {
    install_dir().join("agentplug-runner.version")
}

pub fn local_build_pin_record() -> Option<serde_json::Value> {
    let raw = fs::read_to_string(local_build_pin_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn strip_version_file_bom(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed.strip_prefix('\u{feff}').unwrap_or(trimmed).trim()
}

pub fn installed_runner_version() -> Option<String> {
    fs::read_to_string(runner_version_path())
        .ok()
        .map(|s| strip_version_file_bom(&s).to_string())
        .filter(|s| !s.is_empty())
}

const NO_SELF_UPDATE_ENV: &str = "AGENTPLUG_NO_SELF_UPDATE";
const ALLOW_UPDATE_OVER_LOCAL_BUILD_ENV: &str = "AGENTPLUG_ALLOW_UPDATE_OVER_LOCAL_BUILD";
const BLOCKED_REPORT_QUIET_MS: u64 = 60 * 60 * 1000;

static LAST_BLOCKED_REPORT_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn no_self_update_file() -> PathBuf {
    install_dir().join("agentplug-runner.no-self-update")
}

fn local_build_pin_path() -> PathBuf {
    install_dir().join("agentplug-runner.local-build.json")
}

pub(crate) fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

pub fn update_over_local_build_allowed() -> bool {
    env_flag_enabled(ALLOW_UPDATE_OVER_LOCAL_BUILD_ENV)
}

fn is_staged_runner_suffix(extension: &str) -> bool {
    extension
        .to_ascii_lowercase()
        .strip_prefix("new")
        .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
}

pub fn canonical_runner_exe() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    while path
        .extension()
        .map(|e| is_staged_runner_suffix(&e.to_string_lossy()))
        .unwrap_or(false)
    {
        path = path.with_extension("");
    }
    Some(path)
}

fn running_from_staged_copy() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
        })
        .map(|e| is_staged_runner_suffix(&e))
        .unwrap_or(false)
}

fn write_local_build_pin(canonical: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(canonical)?;
    let sha = sha256_hex(&bytes);
    let record = serde_json::json!({
        "exe": canonical.display().to_string(),
        "sha256": sha,
        "version": env!("CARGO_PKG_VERSION"),
        "commit": crate::build_info::COMMIT,
        "ts": now_ms_for_marker(),
    });
    if let Some(parent) = local_build_pin_path().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(local_build_pin_path(), record.to_string())?;
    Ok(sha)
}

pub fn pin_local_build() -> anyhow::Result<String> {
    let canonical = canonical_runner_exe()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the running runner's canonical path"))?;
    write_local_build_pin(&canonical)
}

pub fn unpin_local_build() -> anyhow::Result<()> {
    match fs::remove_file(local_build_pin_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn sync_local_build_pin() {
    if crate::build_info::is_release_build() {
        if !running_from_staged_copy() {
            let _ = fs::remove_file(local_build_pin_path());
        }
        return;
    }
    if let Some(canonical) = canonical_runner_exe() {
        let _ = write_local_build_pin(&canonical);
    }
}

fn pinned_local_build_reason(canonical: &Path) -> Option<String> {
    let raw = fs::read_to_string(local_build_pin_path()).ok()?;
    let record: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pinned_sha = record.get("sha256")?.as_str()?;
    let bytes = fs::read(canonical).ok()?;
    if !sha256_hex(&bytes).eq_ignore_ascii_case(pinned_sha) {
        return None;
    }
    let commit = record
        .get("commit")
        .and_then(|c| c.as_str())
        .map(|c| format!(", commit {c}"))
        .unwrap_or_default();
    Some(format!(
        "the installed runner at {} is pinned as a locally built binary (sha256 {pinned_sha}{commit})",
        canonical.display()
    ))
}

pub fn installed_runner_blocks_promotion(canonical: &Path) -> Option<String> {
    if update_over_local_build_allowed() {
        return None;
    }
    if let Some(reason) = pinned_local_build_reason(canonical) {
        return Some(reason);
    }
    match crate::build_info::probe(canonical) {
        Some(reported) if !reported.release_build => Some(format!(
            "it reports itself as a locally built binary (version {}, commit {}, built at unix {})",
            reported.version, reported.commit, reported.build_ts
        )),
        _ => None,
    }
}

fn locally_built_installed_reason() -> Option<String> {
    if update_over_local_build_allowed() {
        return None;
    }
    if !crate::build_info::is_release_build() {
        return Some(format!(
            "this runner is a locally built binary (version {}, commit {}, built at unix {}) -- a release is never swapped over a local build",
            env!("CARGO_PKG_VERSION"),
            crate::build_info::COMMIT,
            crate::build_info::BUILD_TS
        ));
    }
    let canonical = canonical_runner_exe()?;
    let current = std::env::current_exe().ok()?;
    if canonical == current {
        return None;
    }
    installed_runner_blocks_promotion(&canonical)
}

fn compare_release_semver(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    if !is_recognized_release_semver(a) || !is_recognized_release_semver(b) {
        return None;
    }
    let parts = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    Some(parts(a).cmp(&parts(b)))
}

pub fn self_update_blocked_reason(latest: &str) -> Option<String> {
    if let Ok(value) = std::env::var(NO_SELF_UPDATE_ENV) {
        let value = value.trim().to_string();
        if !value.is_empty()
            && !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        {
            return Some(format!(
                "{NO_SELF_UPDATE_ENV}={value} is set -- runner self-update is frozen"
            ));
        }
    }
    if no_self_update_file().exists() {
        return Some(format!(
            "{} exists -- runner self-update is frozen",
            no_self_update_file().display()
        ));
    }
    if let Some(reason) = locally_built_installed_reason() {
        return Some(reason);
    }
    let running = env!("CARGO_PKG_VERSION");
    if !is_recognized_release_semver(latest) {
        return Some(format!(
            "latest release tag {latest:?} is not X.Y.Z semver -- refusing to compare it against the running {running}"
        ));
    }
    if !is_recognized_release_semver(running) {
        return None;
    }
    match compare_release_semver(latest, running) {
        Some(std::cmp::Ordering::Greater) => None,
        Some(_) => Some(format!(
            "release {latest} is not strictly newer than the running {running}"
        )),
        None => Some(format!(
            "could not order release {latest} against the running {running}"
        )),
    }
}

fn report_self_update_blocked(reason: &str) {
    let now = now_ms_for_marker();
    let previous = LAST_BLOCKED_REPORT_TS.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(previous) < BLOCKED_REPORT_QUIET_MS {
        return;
    }
    LAST_BLOCKED_REPORT_TS.store(now, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[agentplug runner-update] self-update skipped: {reason}");
}

pub fn fetch_latest_runner_version() -> anyhow::Result<Option<String>> {
    if runner_asset_name().is_none() {
        eprintln!(
            "[agentplug runner-update] self-update unsupported on {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return Ok(None);
    }
    let url = format!("https://api.github.com/repos/{RUNNER_BIN_REPO}/releases/latest");
    let Some(asset) = runner_asset_name() else {
        return Ok(None);
    };
    let probe_url =
        format!("https://github.com/{RUNNER_BIN_REPO}/releases/latest/download/{asset}.sha256");
    if let Ok(resp) = agentplug_host::shared_agent().get(&probe_url).call() {
        let resolved_url = resp.get_url().to_string();
        if let Some(version) = extract_version_from_release_url(&resolved_url)
            .or_else(|| resolve_latest_tag_via_release_page(RUNNER_BIN_REPO))
        {
            return Ok(Some(version));
        }
    }
    match github_api_call(&url) {
        Ok(resp) => {
            let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
            match body.get("tag_name").and_then(|value| value.as_str()) {
                Some(tag) => release_version_from_tag(tag).map(Some).ok_or_else(|| {
                    anyhow::anyhow!("latest runner release tag {tag:?} is not vX.Y.Z semver")
                }),
                None => Ok(None),
            }
        }
        Err(api_err) => Err(describe_github_api_error(&url, api_err)),
    }
}

pub fn stage_runner_self_update() -> anyhow::Result<Option<(PathBuf, String)>> {
    let Some(asset) = runner_asset_name() else {
        return Ok(None);
    };
    let Some(latest) = fetch_latest_runner_version()? else {
        return Ok(None);
    };
    if let Some(reason) = self_update_blocked_reason(&latest) {
        report_self_update_blocked(&reason);
        return Ok(None);
    }
    if marker_is_trustworthy_and_current(latest.as_str()) {
        return Ok(None);
    }
    let running_from_staged_path = running_from_staged_copy();
    let current_exe = canonical_runner_exe().ok_or_else(|| {
        anyhow::anyhow!("cannot resolve the canonical runner path to stage {latest} beside")
    })?;
    let staged_suffix = if running_from_staged_path {
        "new2"
    } else {
        "new"
    };
    let staged = current_exe.with_extension(
        current_exe
            .extension()
            .map(|e| format!("{}.{staged_suffix}", e.to_string_lossy()))
            .unwrap_or_else(|| staged_suffix.to_string()),
    );
    let staged_marker = PathBuf::from(format!("{}.json", staged.display()));
    if staged.exists() {
        if let Ok(raw) = fs::read_to_string(&staged_marker) {
            let recorded = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .as_ref()
                .and_then(|marker| marker.get("version"))
                .and_then(|version| version.as_str())
                .map(str::to_string);
            if recorded.as_deref() == Some(latest.as_str()) {
                return Ok(Some((staged, latest)));
            }
        }
    }
    let base = format!("https://github.com/{RUNNER_BIN_REPO}/releases/download/v{latest}");
    let sha_line = agentplug_host::shared_agent()
        .get(&format!("{base}/{asset}.sha256"))
        .call()?
        .into_string()?;
    let expected_sha = sha_line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {asset} at {base}"))?
        .to_string();
    download_and_verify(&format!("{base}/{asset}"), &staged, &expected_sha)?;
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&staged)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&staged, perms)?;
    }
    let _ = fs::write(
        &staged_marker,
        serde_json::json!({
            "version": latest,
            "asset": asset,
            "sha256": expected_sha,
        })
        .to_string(),
    );
    Ok(Some((staged, latest)))
}

fn marker_is_trustworthy_and_current(latest: &str) -> bool {
    let Some(marker) = installed_runner_version() else {
        return false;
    };
    if marker.trim_start_matches('v') != latest {
        return false;
    }
    let running = env!("CARGO_PKG_VERSION");
    if marker == running {
        return true;
    }
    eprintln!(
        "[agentplug runner-update] version marker claims {marker} but this process is {running} -- a prior takeover recorded the version without completing the swap; correcting the marker and re-staging"
    );
    let _ = record_runner_version(running);
    false
}

pub fn record_runner_version(version: &str) -> anyhow::Result<()> {
    fs::create_dir_all(install_dir())?;
    fs::write(runner_version_path(), version)?;
    sync_local_build_pin();
    Ok(())
}

pub struct LatestPluginRelease {
    pub version: String,
    pub sha256: String,
}

// One redirect probe per plugin per poll. github.com answers `releases/latest/download/<asset>` with a
// redirect whose target names the tag, and the followed sidecar is one line, so the check reads no
// release listing and spends no REST API quota.
fn probe_latest_plugin_release(plugin_name: &str) -> anyhow::Result<Option<LatestPluginRelease>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let probe_url = format!(
        "https://github.com/{}/releases/latest/download/{}.wasm.sha256",
        spec.repo, spec.asset_basename
    );
    let response = agentplug_host::shared_agent().get(&probe_url).call()?;
    let resolved = response.get_url().to_string();
    let Some(version) = extract_version_from_release_url(&resolved) else {
        return Ok(None);
    };
    let sha_line = response.into_string()?;
    let sha256 = sha_line.split_whitespace().next().unwrap_or_default().to_string();
    if sha256.is_empty() {
        anyhow::bail!("empty sha256 sidecar for {} at {resolved}", spec.asset_basename);
    }
    Ok(Some(LatestPluginRelease { version, sha256 }))
}

pub fn fetch_latest_plugin_version(plugin_name: &str) -> anyhow::Result<Option<String>> {
    if let Ok(Some(release)) = probe_latest_plugin_release(plugin_name) {
        return Ok(Some(release.version));
    }
    fetch_latest_plugin_version_by_listing(plugin_name)
}

fn fetch_latest_plugin_version_by_listing(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let wanted_basenames: [String; 2] = [
        spec.asset_basename.clone(),
        if spec.asset_basename == "plugkit-slim" {
            "plugkit".to_string()
        } else {
            spec.asset_basename.clone()
        },
    ];
    let mut latest: Option<String> = None;
    for page in 1..=1_000_u16 {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page=100&page={page}",
            spec.repo
        );
        let resp = github_api_call(&url).map_err(|e| describe_github_api_error(&url, e))?;
        let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
        let Some(releases) = body.as_array() else {
            anyhow::bail!("unexpected releases-list response shape for {}", spec.repo);
        };
        for release in releases {
            let Some(version) = release_candidate_version(release, &wanted_basenames) else {
                continue;
            };
            retain_newer_release_version(&mut latest, version);
        }
        if releases.len() < 100 {
            return Ok(latest);
        }
    }
    anyhow::bail!(
        "release discovery for {} exceeded 1000 full GitHub API pages; refusing a potentially truncated result",
        spec.repo
    )
}

pub fn installed_plugin_version(plugin_name: &str) -> Option<String> {
    fs::read_to_string(plugin_version_path(plugin_name))
        .ok()
        .map(|s| strip_version_file_bom(&s).to_string())
        .filter(|s| !s.is_empty())
}

pub fn is_recognized_release_semver(version: &str) -> bool {
    let version = strip_version_file_bom(version);
    let segments: Vec<&str> = version.split('.').collect();
    segments.len() == 3
        && segments
            .iter()
            .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

fn local_dev_sideload_marker_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.local-dev-sideload.json"))
}

fn warn_local_dev_sideload_loudly(plugin_name: &str, installed: &str) {
    eprintln!(
        "[agentplug daemon] plugin {plugin_name} is served from a NON-RELEASE marker ({installed:?}, not X.Y.Z semver) -- this looks like an intentional local-dev sideload at {}. The auto-updater will NOT overwrite it and will keep skipping every future poll until the marker is changed to a real release version or the sideload is removed. This is expected behavior for a developer build, but it means {plugin_name} is running code that is NOT the latest released version and version drift will be silent unless this warning (or the recorded sideload marker file) is checked.",
        plugin_wasm_path(plugin_name).display()
    );
    let _ = fs::write(
        local_dev_sideload_marker_path(plugin_name),
        serde_json::json!({
            "plugin": plugin_name,
            "installed_marker": installed,
            "detected_ts": crate::download::now_ms_for_marker(),
            "note": "installed .version file is not recognized X.Y.Z semver; treated as an intentional local-dev sideload and never auto-overwritten",
        })
        .to_string(),
    );
}

fn now_ms_for_marker() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn read_local_dev_sideload_marker(plugin_name: &str) -> Option<serde_json::Value> {
    fs::read_to_string(local_dev_sideload_marker_path(plugin_name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn clear_local_dev_sideload_marker(plugin_name: &str) {
    let _ = fs::remove_file(local_dev_sideload_marker_path(plugin_name));
}

fn known_bad_version_marker_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.known-bad-versions.json"))
}

pub fn clear_all_known_bad_version_markers() {
    for plugin_name in ["gm", "bert", "libsql", "treesitter"] {
        let path = known_bad_version_marker_path(plugin_name);
        if path.exists() {
            match fs::remove_file(&path) {
                Ok(()) => eprintln!("[agentplug daemon] cleared known-bad-versions marker for {plugin_name} after runner self-update"),
                Err(e) => eprintln!("[agentplug daemon] failed to clear known-bad-versions marker for {plugin_name}: {e}"),
            }
        }
    }
}

fn read_known_bad_versions(plugin_name: &str) -> std::collections::HashSet<String> {
    fs::read_to_string(known_bad_version_marker_path(plugin_name))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn record_known_bad_version(plugin_name: &str, version: &str) {
    let mut versions = read_known_bad_versions(plugin_name);
    if versions.insert(version.to_string()) {
        let mut sorted: Vec<&String> = versions.iter().collect();
        sorted.sort();
        let _ = fs::write(
            known_bad_version_marker_path(plugin_name),
            serde_json::to_string(&sorted).unwrap_or_default(),
        );
    }
}

fn fetch_remote_wasm_sha256(plugin_name: &str, version: &str) -> anyhow::Result<String> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let base = format!(
        "https://github.com/{}/releases/download/v{version}",
        spec.repo
    );
    let sha_line = agentplug_host::shared_agent()
        .get(&format!("{base}/{}.wasm.sha256", spec.asset_basename))
        .call()?
        .into_string()?;
    sha_line
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!("empty sha256 sidecar for {} at {base}", spec.asset_basename)
        })
}

fn installed_wasm_sha256(plugin_name: &str) -> Option<String> {
    fs::read(plugin_wasm_path(plugin_name))
        .ok()
        .map(|bytes| sha256_hex(&bytes))
}

pub fn refresh_plugin_if_stale(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(installed) = installed_plugin_version(plugin_name) else {
        return Ok(None);
    };
    if !is_recognized_release_semver(&installed) {
        warn_local_dev_sideload_loudly(plugin_name, &installed);
        return Ok(None);
    }
    clear_local_dev_sideload_marker(plugin_name);
    if plugin_asset_spec(plugin_name).is_none() {
        return Ok(None);
    }
    let Some(release) = probe_latest_plugin_release(plugin_name)? else {
        return Ok(None);
    };
    let latest = release.version.clone();
    match compare_release_semver(&latest, &installed) {
        Some(std::cmp::Ordering::Greater) => {}
        Some(std::cmp::Ordering::Equal) => match installed_wasm_sha256(plugin_name) {
            Some(local_sha) if !release.sha256.eq_ignore_ascii_case(&local_sha) => {
                eprintln!("[agentplug daemon] plugin {plugin_name} version {latest} matches but the released asset's sha256 has changed ({} vs installed {local_sha}) -- re-fetching under the same tag", release.sha256);
            }
            _ => return Ok(None),
        },
        Some(std::cmp::Ordering::Less) => {
            eprintln!(
                "[agentplug daemon] plugin {plugin_name} latest release {latest} is older than installed {installed} -- refusing downgrade"
            );
            return Ok(None);
        }
        None => {
            eprintln!(
                "[agentplug daemon] plugin {plugin_name} latest release {latest:?} is not X.Y.Z semver -- refusing update"
            );
            return Ok(None);
        }
    }
    if read_known_bad_versions(plugin_name).contains(&latest) {
        eprintln!(
            "[agentplug daemon] plugin {plugin_name} latest release {latest} is a previously-recorded known-bad version for this runner (host ABI mismatch) -- staying on {installed} until either a newer release appears or this runner updates"
        );
        return Ok(None);
    }
    ensure_plugin_installed(plugin_name, Some(&latest))?;
    if plugin_name == "gm" {
        if let Err(e) = refresh_installed_skills() {
            eprintln!("[agentplug daemon] skill refresh after gm plugin update to {latest} failed: {e:#}");
        }
    }
    Ok(Some(latest))
}

pub fn record_plugin_load_failure_and_rollback(plugin_name: &str) -> anyhow::Result<bool> {
    if let Some(failed_version) = installed_plugin_version(plugin_name) {
        record_known_bad_version(plugin_name, &failed_version);
    }
    let dest = plugin_wasm_path(plugin_name);
    let prev_dest = dest.with_extension("wasm.prev");
    if !prev_dest.exists() {
        return Ok(false);
    }
    restore_wasm_atomically(&prev_dest, &dest)?;
    let version_file = plugin_version_path(plugin_name);
    let prev_version_file = version_file.with_extension("version.prev");
    if prev_version_file.exists() {
        let _ = fs::copy(&prev_version_file, &version_file);
    }
    eprintln!(
        "[agentplug daemon] plugin {plugin_name} rolled back to its previous working version ({} restored from {})",
        dest.display(),
        prev_dest.display()
    );
    Ok(true)
}

// Installed-skill refresh. `skills/manifest.sha256` on the gm `main` branch lists every
// file under `skills/` as `<sha256>  <path>`. Each cycle fetches that one file and
// re-fetches only the skill files whose digest differs, so a copy install tracks its
// SKILL.md, prose, docs, references and data, and gains skills published later. A link is
// the source of truth and is never written. Ownership of a copy is the install ledger
// `.gm-skill-install-modes`; a copy the manifest no longer lists is parked, not deleted.
const SKILL_REMOTE_REPO: &str = "AnEntrypoint/gm";
const SKILL_REMOTE_BRANCH: &str = "main";
const SKILL_MANIFEST_REL: &str = "manifest.sha256";
const SKILL_LEDGER_FILE: &str = ".gm-skill-install-modes";
const SKILL_PRELINK_DIR: &str = ".gm-skill-prelink";
const SKILL_FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SKILL_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
pub const SKILL_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

fn skill_remote_url(rel: &str) -> String {
    format!("https://raw.githubusercontent.com/{SKILL_REMOTE_REPO}/{SKILL_REMOTE_BRANCH}/skills/{rel}")
}

fn fetch_skill_bytes(rel: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let response = agentplug_host::shared_agent().get(&skill_remote_url(rel)).call()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("{rel} is larger than {max_bytes} bytes");
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillManifestEntry {
    /// The skill directory name, the first path component under `skills/`.
    pub skill: String,
    /// The path inside that skill directory, `/`-separated.
    pub inner: String,
    pub sha256: String,
}

fn is_safe_skill_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && !rel.contains(':')
        && rel
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

pub fn parse_skill_manifest(text: &str) -> anyhow::Result<Vec<SkillManifestEntry>> {
    let mut entries = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let (digest, rel) = line
            .split_once("  ")
            .ok_or_else(|| anyhow::anyhow!("manifest line {number} is not '<sha256>  <path>'"))?;
        let lowercase_hex = digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !lowercase_hex {
            anyhow::bail!("manifest line {number} has no lowercase sha256 digest");
        }
        if !is_safe_skill_path(rel) {
            anyhow::bail!("manifest line {number} names an unsafe path {rel:?}");
        }
        // A file directly under skills/ (skill-assets.txt) belongs to no skill directory.
        let Some((skill, inner)) = rel.split_once('/') else {
            continue;
        };
        entries.push(SkillManifestEntry {
            skill: skill.to_string(),
            inner: inner.to_string(),
            sha256: digest.to_string(),
        });
    }
    Ok(entries)
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn local_sha256(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| sha256_hex(&bytes))
}

fn write_skill_file(target: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("skill-file");
    let tmp = target.with_file_name(format!("{name}.tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

fn skill_ledger_path(root: &Path) -> PathBuf {
    root.join(SKILL_LEDGER_FILE)
}

fn skill_ledger_lines(root: &Path) -> Vec<String> {
    fs::read_to_string(skill_ledger_path(root))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn skill_ledger_fields(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    Some((parts.next()?, parts.next()?))
}

/// Replaces the ledger line for `name` with `name <mode> <unix-ms> <source>`, or removes it
/// when `mode` is None. The line format matches the one `bin/gm-install.js` writes.
fn update_skill_ledger(root: &Path, name: &str, mode: Option<(&str, &str)>) -> anyhow::Result<()> {
    let mut lines: Vec<String> = skill_ledger_lines(root)
        .into_iter()
        .filter(|line| skill_ledger_fields(line).map(|(n, _)| n) != Some(name))
        .collect();
    if let Some((kind, source)) = mode {
        lines.push(format!("{name} {kind} {} {source}", unix_ms()));
    }
    let body = format!(
        "# gm skill install modes: <name> <linked|copied|refused> <unix-ms> <source>\n{}\n",
        lines.join("\n")
    );
    let file = skill_ledger_path(root);
    let tmp = file.with_file_name(format!("{SKILL_LEDGER_FILE}.tmp.{}", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &file)?;
    Ok(())
}

/// Moves `from` (inside `root`) to the dotted prelink park, keeping its path under `root`.
/// A dotted directory is never served as a skill, and nothing is deleted.
fn park_under_prelink(root: &Path, from: &Path) -> anyhow::Result<PathBuf> {
    let rel = from.strip_prefix(root)?;
    let to = root
        .join(SKILL_PRELINK_DIR)
        .join(unix_ms().to_string())
        .join(rel);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, &to)?;
    Ok(to)
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            files_under(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct SkillRefreshReport {
    pub written: Vec<PathBuf>,
    pub installed: Vec<String>,
    pub parked: Vec<PathBuf>,
    pub linked: usize,
    pub failures: Vec<String>,
}

impl SkillRefreshReport {
    pub fn changed(&self) -> bool {
        !(self.written.is_empty()
            && self.installed.is_empty()
            && self.parked.is_empty()
            && self.failures.is_empty())
    }
}

/// Applies `entries` to every install root. `fetch` returns the bytes of `<skill>/<inner>`.
/// A root that does not exist is left alone. A missing skill is installed only into a root
/// whose `gm` is a real copy, which is what marks the root as gm-managed.
pub fn refresh_skill_roots(
    roots: &[PathBuf],
    entries: &[SkillManifestEntry],
    fetch: &dyn Fn(&str) -> anyhow::Result<Vec<u8>>,
) -> SkillRefreshReport {
    let mut report = SkillRefreshReport::default();
    let mut wanted: std::collections::BTreeMap<&str, Vec<&SkillManifestEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        wanted.entry(entry.skill.as_str()).or_default().push(entry);
    }
    let publishes_gm = wanted
        .get("gm")
        .is_some_and(|files| files.iter().any(|f| f.inner == "SKILL.md"));

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let anchored = publishes_gm
            && !is_link(&root.join("gm"))
            && root.join("gm").join("SKILL.md").is_file();

        for (skill, files) in &wanted {
            let dir = root.join(skill);
            if is_link(&dir) {
                report.linked += 1;
                continue;
            }
            let fresh = !dir.exists();
            if fresh && !anchored {
                continue;
            }

            for file in files {
                let target = dir.join(&file.inner);
                if local_sha256(&target).as_deref() == Some(file.sha256.as_str()) {
                    continue;
                }
                let outcome = fetch(&format!("{skill}/{}", file.inner)).and_then(|bytes| {
                    let got = sha256_hex(&bytes);
                    if got != file.sha256 {
                        anyhow::bail!(
                            "sha256 {got} does not match the manifest digest {}",
                            file.sha256
                        );
                    }
                    write_skill_file(&target, &bytes)
                });
                match outcome {
                    Ok(()) => report.written.push(target),
                    Err(e) => report
                        .failures
                        .push(format!("{skill}/{}: {e:#}", file.inner)),
                }
            }

            let listed: std::collections::HashSet<&str> =
                files.iter().map(|f| f.inner.as_str()).collect();
            let mut present = Vec::new();
            if dir.is_dir() {
                if let Err(e) = files_under(&dir, &mut present) {
                    report
                        .failures
                        .push(format!("{skill}: listing failed: {e:#}"));
                }
            }
            for path in present {
                let inner = path
                    .strip_prefix(&dir)
                    .map(|p| p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
                    .unwrap_or_default();
                if listed.contains(inner.as_str()) {
                    continue;
                }
                match park_under_prelink(root, &path) {
                    Ok(parked) => report.parked.push(parked),
                    Err(e) => report
                        .failures
                        .push(format!("{skill}/{inner}: park failed: {e:#}")),
                }
            }

            if fresh && dir.exists() {
                match update_skill_ledger(root, skill, Some(("copied", "manifest:main"))) {
                    Ok(()) => report.installed.push(skill.to_string()),
                    Err(e) => report
                        .failures
                        .push(format!("{skill}: ledger write failed: {e:#}")),
                }
            }
        }

        if publishes_gm {
            for line in skill_ledger_lines(root) {
                let Some((name, mode)) = skill_ledger_fields(&line) else {
                    continue;
                };
                if mode != "copied" || wanted.contains_key(name) {
                    continue;
                }
                let dir = root.join(name);
                if is_link(&dir) {
                    continue;
                }
                if dir.exists() {
                    match park_under_prelink(root, &dir) {
                        Ok(parked) => report.parked.push(parked),
                        Err(e) => {
                            report
                                .failures
                                .push(format!("{name}: retired copy could not be parked: {e:#}"));
                            continue;
                        }
                    }
                }
                let _ = update_skill_ledger(root, name, None);
            }
        }
    }
    report
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn installed_skill_roots(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".agents").join("skills"),
        home.join(".claude").join("skills"),
    ]
}

const SKILL_MANIFEST_CACHE_FILE: &str = "skill-manifest.cache.json";

// One conditional request per cycle. An unchanged manifest answers 304 with no body, so the cached
// copy is reused and only a changed manifest moves its bytes.
fn fetch_skill_manifest_text() -> anyhow::Result<String> {
    let cache_path = agentplug_host::install_dir().join(SKILL_MANIFEST_CACHE_FILE);
    let cached: Option<serde_json::Value> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let cached_etag = cached
        .as_ref()
        .and_then(|value| value.get("etag"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let cached_text = cached
        .as_ref()
        .and_then(|value| value.get("text"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut request = agentplug_host::shared_agent().get(&skill_remote_url(SKILL_MANIFEST_REL));
    if let (Some(etag), Some(_)) = (&cached_etag, &cached_text) {
        request = request.set("If-None-Match", etag);
    }
    let response = request.call()?;
    if response.status() == 304 {
        return cached_text
            .ok_or_else(|| anyhow::anyhow!("skill manifest answered 304 with no cached copy"));
    }
    let etag = response.header("ETag").map(str::to_string);
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(SKILL_MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SKILL_MANIFEST_MAX_BYTES {
        anyhow::bail!("skill manifest exceeds {SKILL_MANIFEST_MAX_BYTES} bytes");
    }
    let text = String::from_utf8(bytes)?;
    if let Some(etag) = etag {
        let record = serde_json::json!({ "etag": etag, "text": text });
        let _ = fs::write(&cache_path, record.to_string());
    }
    Ok(text)
}

pub fn refresh_installed_skills() -> anyhow::Result<SkillRefreshReport> {
    let Some(home) = home_dir() else {
        anyhow::bail!("no HOME/USERPROFILE set -- cannot locate installed skills directories");
    };
    let manifest = fetch_skill_manifest_text()?;
    let entries = parse_skill_manifest(&manifest)?;
    if entries.is_empty() {
        anyhow::bail!("the published skill manifest lists no files; refusing to prune installs against it");
    }
    let report = refresh_skill_roots(&installed_skill_roots(&home), &entries, &|rel| {
        fetch_skill_bytes(rel, SKILL_FILE_MAX_BYTES)
    });
    if report.changed() {
        eprintln!(
            "[agentplug daemon] skills refreshed: {} file(s) written, installed {:?}, parked {} stale path(s), {} linked skip(s), {} failure(s){}",
            report.written.len(),
            report.installed,
            report.parked.len(),
            report.linked,
            report.failures.len(),
            if report.failures.is_empty() {
                String::new()
            } else {
                format!(": {}", report.failures.join("; "))
            }
        );
    }
    Ok(report)
}


#[cfg(test)]
mod skill_refresh_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skill-refresh-{tag}-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest_line(bytes: &[u8], rel: &str) -> String {
        format!("{}  {rel}", sha256_hex(bytes))
    }

    #[test]
    fn manifest_accepts_safe_paths_and_refuses_the_rest() {
        let ok = format!(
            "{}  gm/SKILL.md\n# comment\n\n{}  gm/prose/a.md\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let entries = parse_skill_manifest(&ok).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].skill, "gm");
        assert_eq!(entries[1].inner, "prose/a.md");
        let top_level = format!("{}  skill-assets.txt\n", "c".repeat(64));
        assert!(parse_skill_manifest(&top_level).unwrap().is_empty());
        let digest = "a".repeat(64);
        for bad in [
            format!("{digest}  ../etc/passwd"),
            format!("{digest}  /abs/path"),
            format!("{digest}  gm\\SKILL.md"),
            format!("{digest}  gm/../../escape"),
            format!("{}  gm/SKILL.md", "A".repeat(64)),
            format!("{}  gm/SKILL.md", "a".repeat(63)),
        ] {
            assert!(parse_skill_manifest(&bad).is_err(), "accepted {bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refresh_updates_prose_installs_new_skills_parks_stale_and_keeps_links() {
        let home = scratch("home");
        let claude = home.join(".claude").join("skills");
        let agents = home.join(".agents").join("skills");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&agents).unwrap();

        let gm = claude.join("gm");
        fs::create_dir_all(gm.join("prose")).unwrap();
        fs::write(gm.join("SKILL.md"), b"old skill").unwrap();
        fs::write(gm.join("prose").join("a.md"), b"old prose").unwrap();
        fs::write(gm.join("prose").join("gone.md"), b"dropped").unwrap();

        let retired = claude.join("gm-retired");
        fs::create_dir_all(&retired).unwrap();
        fs::write(retired.join("SKILL.md"), b"retired").unwrap();
        update_skill_ledger(&claude, "gm-retired", Some(("copied", "manifest:main"))).unwrap();

        let own = claude.join("my-own");
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("SKILL.md"), b"mine").unwrap();

        let checkout = home.join("checkout").join("linked");
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("SKILL.md"), b"checkout").unwrap();
        std::os::unix::fs::symlink(&checkout, claude.join("linked")).unwrap();

        let skill_md = b"new skill".to_vec();
        let prose_a = b"new prose".to_vec();
        let prose_b = b"brand new prose".to_vec();
        let newskill = b"installed skill".to_vec();
        let tampered_expected = b"expected bytes".to_vec();
        let manifest = [
            manifest_line(&skill_md, "gm/SKILL.md"),
            manifest_line(&prose_a, "gm/prose/a.md"),
            manifest_line(&prose_b, "gm/prose/b.md"),
            manifest_line(&newskill, "newskill/SKILL.md"),
            manifest_line(&tampered_expected, "gm/prose/c.md"),
            manifest_line(b"linked remote", "linked/SKILL.md"),
        ]
        .join("\n");
        let entries = parse_skill_manifest(&manifest).unwrap();
        let fetch = |rel: &str| -> anyhow::Result<Vec<u8>> {
            match rel {
                "gm/SKILL.md" => Ok(skill_md.clone()),
                "gm/prose/a.md" => Ok(prose_a.clone()),
                "gm/prose/b.md" => Ok(prose_b.clone()),
                "newskill/SKILL.md" => Ok(newskill.clone()),
                "gm/prose/c.md" => Ok(b"tampered".to_vec()),
                other => anyhow::bail!("unexpected fetch of {other}"),
            }
        };
        let roots = vec![claude.clone(), agents.clone()];

        let report = refresh_skill_roots(&roots, &entries, &fetch);

        assert_eq!(fs::read(gm.join("SKILL.md")).unwrap(), skill_md);
        assert_eq!(fs::read(gm.join("prose").join("a.md")).unwrap(), prose_a);
        assert_eq!(fs::read(gm.join("prose").join("b.md")).unwrap(), prose_b);
        assert!(!gm.join("prose").join("c.md").exists(), "a digest mismatch was written");
        assert!(!gm.join("prose").join("gone.md").exists());
        assert!(report
            .parked
            .iter()
            .any(|p| p.ends_with(Path::new("gm").join("prose").join("gone.md"))));
        assert!(!retired.exists(), "a retired ledger copy was not parked");
        assert!(report.parked.iter().any(|p| p.ends_with("gm-retired")));
        assert!(!skill_ledger_lines(&claude).iter().any(|l| l.starts_with("gm-retired ")));
        assert_eq!(fs::read(own.join("SKILL.md")).unwrap(), b"mine");
        assert_eq!(fs::read(checkout.join("SKILL.md")).unwrap(), b"checkout");
        assert!(is_link(&claude.join("linked")));
        assert_eq!(fs::read(claude.join("newskill").join("SKILL.md")).unwrap(), newskill);
        assert!(report.installed.iter().any(|s| s == "newskill"));
        assert!(skill_ledger_lines(&claude).iter().any(|l| l.starts_with("newskill copied ")));
        assert!(!agents.join("newskill").exists(), "a root without gm got a new skill");
        assert_eq!(report.failures.len(), 1, "failures: {:?}", report.failures);
        assert!(report.failures[0].starts_with("gm/prose/c.md"));
        assert_eq!(report.linked, 1);

        let second = refresh_skill_roots(&roots, &entries, &fetch);
        assert!(second.written.is_empty(), "second cycle rewrote {:?}", second.written);
        assert!(second.installed.is_empty());
        assert!(second.parked.is_empty());
        assert_eq!(second.failures.len(), 1);

        let _ = fs::remove_dir_all(&home);
    }

    /// Live witness, run on demand: `cargo test -p agentplug-runner live_refresh -- --ignored`.
    /// Uses the real manifest and file URLs on gm main against a stale copy install.
    #[test]
    #[ignore]
    fn live_refresh_brings_a_stale_copy_install_to_the_published_main() {
        let home = scratch("live-home");
        std::env::set_var("HOME", &home);
        let claude = home.join(".claude").join("skills");
        let gm = claude.join("gm");
        fs::create_dir_all(gm.join("prose")).unwrap();
        fs::write(gm.join("SKILL.md"), b"stale").unwrap();
        fs::write(gm.join("prose").join("stale-prose.md"), b"stale").unwrap();

        let first = refresh_installed_skills().expect("live refresh");
        assert!(first.failures.is_empty(), "failures: {:?}", first.failures);
        assert!(first.installed.iter().any(|s| s == "dream-rsi"), "installed: {:?}", first.installed);
        let published_skill = fetch_skill_bytes("gm/SKILL.md", SKILL_FILE_MAX_BYTES).unwrap();
        assert_eq!(fs::read(gm.join("SKILL.md")).unwrap(), published_skill);
        assert!(!gm.join("prose").join("stale-prose.md").exists());
        assert!(gm.join("prose").join("acceptport.md").is_file(), "published prose was not installed");

        let second = refresh_installed_skills().expect("second live refresh");
        assert!(second.written.is_empty(), "second cycle rewrote {:?}", second.written);
        assert!(second.parked.is_empty() && second.installed.is_empty());

        let _ = fs::remove_dir_all(&home);
    }
}

const PLUGIN_INSTALL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

fn plugin_install_failure_marker_path(plugin_name: &str) -> PathBuf {
    install_dir()
        .join("plugins")
        .join(format!("{plugin_name}.install-backoff-ts"))
}

fn read_plugin_install_failure_elapsed(plugin_name: &str) -> Option<Duration> {
    let raw = fs::read_to_string(plugin_install_failure_marker_path(plugin_name)).ok()?;
    let failed_at_ms: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_millis(
        now_ms_for_marker().saturating_sub(failed_at_ms),
    ))
}

pub fn ensure_plugin_installed(
    plugin_name: &str,
    explicit_version: Option<&str>,
) -> anyhow::Result<PathBuf> {
    if !is_safe_plugin_name(plugin_name) {
        anyhow::bail!(
            "plugin name {plugin_name:?} is not a safe identifier (expected an ASCII alphanumeric name with optional '.', '_' or '-')"
        );
    }
    if let Some(version) = explicit_version {
        if !is_recognized_release_semver(version) {
            anyhow::bail!("requested plugin version {version:?} is not X.Y.Z semver");
        }
    }
    let dest = plugin_wasm_path(plugin_name);
    if dest.exists() && explicit_version.is_none() {
        return Ok(dest);
    }
    if explicit_version.is_none() {
        if let Some(elapsed) = read_plugin_install_failure_elapsed(plugin_name) {
            if elapsed < PLUGIN_INSTALL_RETRY_COOLDOWN {
                anyhow::bail!(
                    "plugin {plugin_name} install failed {:.0}s ago -- retry backoff active for {:.0}s more",
                    elapsed.as_secs_f64(),
                    (PLUGIN_INSTALL_RETRY_COOLDOWN - elapsed).as_secs_f64()
                );
            }
        }
    }
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let version_file = plugin_version_path(plugin_name);

    let result = match ensure_plugin_installed_via_github(plugin_name, explicit_version, &spec, &dest, &version_file) {
        Ok(path) => Ok(path),
        Err(github_api_err) if explicit_version.is_none() => match try_ensure_plugin_installed_via_direct_release_latest(&spec, &dest, &version_file) {
            Ok(path) => Ok(path),
            Err(direct_err) => Err(anyhow::anyhow!(
                "plugin {plugin_name} install failed on all paths -- GitHub API: {github_api_err:#}; direct release download: {direct_err:#}"
            )),
        },
        Err(github_api_err) => Err(github_api_err),
    };
    if explicit_version.is_none() {
        let marker = plugin_install_failure_marker_path(plugin_name);
        match &result {
            Ok(_) => {
                let _ = fs::remove_file(&marker);
            }
            Err(_) => {
                if let Some(parent) = marker.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&marker, now_ms_for_marker().to_string());
            }
        }
    }
    result
}

fn ensure_plugin_installed_via_github(
    plugin_name: &str,
    explicit_version: Option<&str>,
    spec: &PluginAssetSpec,
    dest: &Path,
    version_file: &Path,
) -> anyhow::Result<PathBuf> {
    let version = match explicit_version {
        Some(v) => v.to_string(),
        None => fetch_latest_plugin_version(plugin_name)?.ok_or_else(|| {
            anyhow::anyhow!("could not resolve latest version for plugin {plugin_name}")
        })?,
    };

    if dest.exists() {
        if let Ok(installed) = fs::read_to_string(version_file) {
            if installed.trim() == version {
                let sha_drifted = match (
                    fetch_remote_wasm_sha256(plugin_name, &version),
                    installed_wasm_sha256(plugin_name),
                ) {
                    (Ok(remote_sha), Some(local_sha)) => {
                        !remote_sha.eq_ignore_ascii_case(&local_sha)
                    }
                    _ => false,
                };
                if !sha_drifted {
                    return Ok(dest.to_path_buf());
                }
            }
        }
    }

    let base = format!(
        "https://github.com/{}/releases/download/v{version}",
        spec.repo
    );

    let sha_url = format!("{base}/{}.wasm.sha256", spec.asset_basename);
    let mut effective_basename = spec.asset_basename.as_str();
    let sha_resp = match agentplug_host::shared_agent().get(&sha_url).call() {
        Ok(resp) => resp,
        Err(_) if spec.asset_basename == "plugkit-slim" => {
            effective_basename = "plugkit";
            agentplug_host::shared_agent()
                .get(&format!("{base}/plugkit.wasm.sha256"))
                .call()?
        }
        Err(e) => return Err(e.into()),
    };
    let wasm_url = format!("{base}/{effective_basename}.wasm");
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {effective_basename} at {base}"))?
        .to_string();

    snapshot_prev_wasm_and_version(dest, version_file)?;
    if let Some(cached) = shared_plugin_payload(&expected_sha) {
        match link_or_copy_payload(&cached, dest) {
            Ok(()) => {
                mark_shared_payload_used(&expected_sha);
                eprintln!(
                    "[agentplug] plugin {plugin_name} {version} served from the shared payload cache ({}), no download needed",
                    cached.display()
                );
                record_plugin_install(dest, version_file, &version)?;
                write_plugin_build_record(plugin_name, &spec.repo, &version, dest);
                return Ok(dest.to_path_buf());
            }
            Err(e) => eprintln!(
                "[agentplug] plugin {plugin_name}: could not link the cached payload ({e:#}) -- downloading it instead"
            ),
        }
    }
    download_and_verify(&wasm_url, dest, &expected_sha)?;
    populate_shared_plugin_payload(dest, &expected_sha);
    record_plugin_install(dest, version_file, &version)?;
    write_plugin_build_record(plugin_name, &spec.repo, &version, dest);
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool, prerelease: bool, assets: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "published_at": "2026-10-06T00:00:00Z",
            "assets": assets.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn release_metadata_requires_one_published_pair_at_a_prefixed_semver_tag() {
        let wanted = ["bert".to_string()];
        let valid = release("v1.2.3", false, false, &["bert.wasm", "bert.wasm.sha256"]);
        assert_eq!(
            release_candidate_version(&valid, &wanted),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            release_candidate_version(
                &release("1.2.3", false, false, &["bert.wasm", "bert.wasm.sha256"]),
                &wanted
            ),
            None
        );
        assert_eq!(
            release_candidate_version(
                &release("v1.2.3", true, false, &["bert.wasm", "bert.wasm.sha256"]),
                &wanted
            ),
            None
        );
        assert_eq!(
            release_candidate_version(
                &release("v1.2.3", false, true, &["bert.wasm", "bert.wasm.sha256"]),
                &wanted
            ),
            None
        );
        assert_eq!(
            release_candidate_version(&release("v1.2.3", false, false, &["bert.wasm"]), &wanted),
            None
        );
        assert_eq!(
            release_candidate_version(
                &release(
                    "v1.2.3",
                    false,
                    false,
                    &["bert.wasm", "bert.wasm", "bert.wasm.sha256"]
                ),
                &wanted
            ),
            None
        );
    }

    #[test]
    fn release_metadata_keeps_the_highest_valid_version_across_pages() {
        let mut latest = None;
        retain_newer_release_version(&mut latest, "1.2.3".to_string());
        retain_newer_release_version(&mut latest, "1.10.0".to_string());
        retain_newer_release_version(&mut latest, "1.9.9".to_string());
        assert_eq!(latest, Some("1.10.0".to_string()));
    }

    #[test]
    fn release_tags_require_the_download_url_convention() {
        assert_eq!(
            release_version_from_tag("v1.2.3"),
            Some("1.2.3".to_string())
        );
        assert_eq!(release_version_from_tag("1.2.3"), None);
        assert_eq!(release_version_from_tag("vv1.2.3"), None);
        assert_eq!(release_version_from_tag("v1.2"), None);
    }

    #[test]
    fn skill_refresh_reads_a_link_as_a_link_and_not_as_its_target() {
        let root = std::env::temp_dir().join(format!(
            "agentplug-skill-link-test-{}-{}",
            std::process::id(),
            now_ms_for_marker()
        ));
        fs::create_dir_all(&root).unwrap();
        let dir = root.join("gm");
        fs::create_dir_all(&dir).unwrap();

        assert!(!is_link(&dir), "a real directory is not a link");
        assert!(!is_link(&root.join("absent")), "an absent path is not a link");

        #[cfg(not(windows))]
        {
            let link = root.join("gm-link");
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert!(is_link(&link), "a symlink to a directory is a link");
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runner_asset_matrix_accepts_only_published_host_targets() {
        assert_eq!(
            runner_asset_name_for("windows", "x86_64"),
            Some("agentplug-runner-windows-x64.exe")
        );
        assert_eq!(
            runner_asset_name_for("windows", "aarch64"),
            Some("agentplug-runner-windows-arm64.exe")
        );
        assert_eq!(
            runner_asset_name_for("macos", "x86_64"),
            Some("agentplug-runner-macos-x64")
        );
        assert_eq!(
            runner_asset_name_for("macos", "aarch64"),
            Some("agentplug-runner-macos-arm64")
        );
        assert_eq!(
            runner_asset_name_for("linux", "x86_64"),
            Some("agentplug-runner-linux-x64")
        );
        assert_eq!(
            runner_asset_name_for("linux", "aarch64"),
            Some("agentplug-runner-linux-arm64")
        );
        assert_eq!(runner_asset_name_for("linux", "arm"), None);
        assert_eq!(runner_asset_name_for("freebsd", "x86_64"), None);
        assert_eq!(runner_asset_name_for("windows", "i686"), None);
    }

    #[test]
    fn download_write_refuses_a_preexisting_tmp_path_without_touching_it() {
        let root = std::env::temp_dir().join(format!(
            "agentplug-download-test-{}-{}",
            std::process::id(),
            now_ms_for_marker()
        ));
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("runner");
        let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, b"preserve").unwrap();

        assert!(write_download_atomically(&dest, b"replacement").is_err());
        assert_eq!(fs::read(&tmp).unwrap(), b"preserve");
        assert!(!dest.exists());

        fs::remove_dir_all(root).unwrap();
    }
}
