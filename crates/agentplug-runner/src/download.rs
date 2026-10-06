use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentplug_host::install_dir;

use crate::update_trust::{self, AssetIdentity, UpdateRejected};

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
}

fn extract_version_from_release_url(url: &str) -> Option<String> {
    let idx = url.find("/releases/download/")?;
    let rest = &url[idx + "/releases/download/".len()..];
    let tag = rest.split('/').next()?;
    if tag.is_empty() {
        return None;
    }
    Some(tag.trim_start_matches('v').to_string())
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
    Some(tag.trim_start_matches('v').to_string())
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
    let artifact = format!("{}.wasm", spec.asset_basename);
    let running = installed_plugin_version_from_file(version_file);
    let identity = AssetIdentity {
        artifact: &artifact,
        version: &version,
        running: running.as_deref(),
    };
    snapshot_prev_wasm_and_version(dest, version_file)?;
    let finalized = download_and_verify(&wasm_url, dest, &expected_sha, &identity)?;
    record_plugin_install(dest, version_file, &version, &identity, &finalized)?;
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

fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty())
}

fn github_api_call(url: &str) -> Result<ureq::Response, ureq::Error> {
    let Some(token) = github_token() else {
        return github_api_request(url).call();
    };
    match github_api_request(url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Err(ureq::Error::Status(401, _)) => {
            eprintln!("[agentplug] GITHUB_TOKEN/GH_TOKEN rejected (401 Bad credentials) fetching {url} -- retrying unauthenticated");
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

pub fn download_and_verify(
    url: &str,
    dest: &Path,
    expected_sha256_hex: &str,
    identity: &AssetIdentity,
) -> anyhow::Result<update_trust::Finalized> {
    let preflight = update_trust::preflight(identity, &format!("{url}.sig"))?;
    let resp = agentplug_host::shared_agent().get(url).call()?;
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
    let finalized = update_trust::finalize(identity, &preflight, &bytes)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    let write_result = (|| -> anyhow::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        if let Err(rm_err) = fs::remove_file(&tmp) {
            if rm_err.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[agentplug] failed to remove incomplete tmp file {}: {rm_err}",
                    tmp.display()
                );
            }
        }
        return Err(e);
    }
    fs::rename(&tmp, dest)?;
    Ok(finalized)
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
        "oxibrowser" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/obrowser-bin".to_string(),
            asset_basename: "oxibrowser".to_string(),
        }),
        "crux" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/agentplug-crux-bin".to_string(),
            asset_basename: "crux".to_string(),
        }),
        "liqology" => Some(PluginAssetSpec {
            repo: "AnEntrypoint/liqology".to_string(),
            asset_basename: "liqology".to_string(),
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

fn installed_plugin_version_from_file(version_file: &Path) -> Option<String> {
    fs::read_to_string(version_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

fn record_plugin_install(
    dest: &Path,
    version_file: &Path,
    version: &str,
    identity: &AssetIdentity,
    finalized: &update_trust::Finalized,
) -> anyhow::Result<()> {
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
    update_trust::installed(identity, finalized);
    Ok(())
}

const RUNNER_BIN_REPO: &str = "AnEntrypoint/agentplug-bin";

pub(crate) fn runner_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("agentplug-runner-windows-x64.exe"),
        ("windows", "aarch64") => Some("agentplug-runner-windows-arm64.exe"),
        ("macos", "x86_64") => Some("agentplug-runner-macos-x64"),
        ("macos", "aarch64") => Some("agentplug-runner-macos-arm64"),
        ("linux", "x86_64") => Some("agentplug-runner-linux-x64"),
        ("linux", "aarch64") => Some("agentplug-runner-linux-arm64"),
        _ => None,
    }
}

fn runner_version_path() -> PathBuf {
    install_dir().join("agentplug-runner.version")
}

pub fn installed_runner_version() -> Option<String> {
    fs::read_to_string(runner_version_path())
        .ok()
        .map(|s| s.trim().to_string())
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

pub fn canonical_runner_exe() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    while path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .map(|e| e == "new" || e == "new2")
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
        .map(|e| e == "new" || e == "new2")
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
    let url = format!("https://api.github.com/repos/{RUNNER_BIN_REPO}/releases/latest");
    match github_api_call(&url) {
        Ok(resp) => {
            let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
            Ok(body
                .get("tag_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_start_matches('v').to_string()))
        }
        Err(api_err) => {
            let Some(asset) = runner_asset_name() else {
                return Err(describe_github_api_error(&url, api_err));
            };
            let probe_url = format!(
                "https://github.com/{RUNNER_BIN_REPO}/releases/latest/download/{asset}.sha256"
            );
            match agentplug_host::shared_agent().get(&probe_url).call() {
                Ok(resp) => {
                    let resolved_url = resp.get_url().to_string();
                    Ok(extract_version_from_release_url(&resolved_url))
                }
                Err(_) => Err(describe_github_api_error(&url, api_err)),
            }
        }
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
    let running_from_staged_path = std::env::current_exe()?
        .extension()
        .map(|e| e.eq_ignore_ascii_case("new"))
        .unwrap_or(false);
    let mut current_exe = std::env::current_exe()?;
    while current_exe
        .extension()
        .map(|e| e.eq_ignore_ascii_case("new"))
        .unwrap_or(false)
    {
        current_exe = current_exe.with_extension("");
    }
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
    let identity = AssetIdentity {
        artifact: asset,
        version: &latest,
        running: Some(env!("CARGO_PKG_VERSION")),
    };
    let finalized = download_and_verify(
        &format!("{base}/{asset}"),
        &staged,
        &expected_sha,
        &identity,
    )?;
    update_trust::record_stage_outcome(&staged, &identity, &finalized);
    update_trust::installed(&identity, &finalized);
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&staged)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&staged, perms)?;
    }
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

pub fn fetch_latest_plugin_version(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page=100",
        spec.repo
    );
    let resp = github_api_call(&url).map_err(|e| describe_github_api_error(&url, e))?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    let Some(releases) = body.as_array() else {
        anyhow::bail!("unexpected releases-list response shape for {}", spec.repo);
    };
    let wanted_names: [String; 2] = [
        format!("{}.wasm", spec.asset_basename),
        if spec.asset_basename == "plugkit-slim" {
            "plugkit.wasm".to_string()
        } else {
            format!("{}.wasm", spec.asset_basename)
        },
    ];
    for release in releases {
        let has_asset = release
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|assets| {
                assets.iter().any(|a| {
                    a.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| wanted_names.iter().any(|w| w == n))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if has_asset {
            return Ok(release
                .get("tag_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_start_matches('v').to_string()));
        }
    }
    Ok(None)
}

pub fn installed_plugin_version(plugin_name: &str) -> Option<String> {
    fs::read_to_string(plugin_version_path(plugin_name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn is_recognized_release_semver(version: &str) -> bool {
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
    let Some(latest) = fetch_latest_plugin_version(plugin_name)? else {
        return Ok(None);
    };
    match compare_release_semver(&latest, &installed) {
        Some(std::cmp::Ordering::Greater) => {}
        Some(std::cmp::Ordering::Equal) => match (
            fetch_remote_wasm_sha256(plugin_name, &latest),
            installed_wasm_sha256(plugin_name),
        ) {
            (Ok(remote_sha), Some(local_sha)) if !remote_sha.eq_ignore_ascii_case(&local_sha) => {
                eprintln!("[agentplug daemon] plugin {plugin_name} version {latest} matches but the released asset's sha256 has changed ({remote_sha} vs installed {local_sha}) -- re-fetching under the same tag");
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
        if let Err(e) = refresh_installed_skill_md() {
            eprintln!("[agentplug daemon] SKILL.md refresh after gm plugin update to {latest} failed: {e:#}");
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
    fs::copy(&prev_dest, &dest)?;
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

const SKILL_MD_REMOTE_REPO: &str = "AnEntrypoint/gm";
const SKILL_MD_REMOTE_BRANCH: &str = "main";

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
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

fn discover_installed_skill_names(home: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = std::collections::BTreeSet::new();
    for root in installed_skill_roots(home) {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if entry.path().join("SKILL.md").exists() {
                if let Some(name) = entry.file_name().to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

fn fetch_remote_skill_md(skill_name: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://raw.githubusercontent.com/{SKILL_MD_REMOTE_REPO}/{SKILL_MD_REMOTE_BRANCH}/skills/{skill_name}/SKILL.md"
    );
    Ok(agentplug_host::shared_agent()
        .get(&url)
        .call()?
        .into_string()?)
}

pub fn refresh_installed_skill_md() -> anyhow::Result<Vec<PathBuf>> {
    let Some(home) = home_dir() else {
        anyhow::bail!("no HOME/USERPROFILE set -- cannot locate installed skills directories");
    };
    let skill_names = discover_installed_skill_names(&home)?;
    if skill_names.is_empty() {
        return Ok(Vec::new());
    }

    let mut refreshed = Vec::new();
    let mut failures = Vec::new();
    for skill_name in skill_names {
        let bundled = match fetch_remote_skill_md(&skill_name) {
            Ok(content) => content,
            Err(e) => {
                failures.push(format!("{skill_name}: {e:#}"));
                continue;
            }
        };
        let bundled_hash = sha256_hex(normalize_newlines(&bundled).as_bytes());

        for root in installed_skill_roots(&home) {
            let target = root.join(&skill_name).join("SKILL.md");
            if !target.exists() {
                continue;
            }
            let needs_write = match fs::read_to_string(&target) {
                Ok(existing) => {
                    sha256_hex(normalize_newlines(&existing).as_bytes()) != bundled_hash
                }
                Err(_) => true,
            };
            if !needs_write {
                continue;
            }
            let tmp = target.with_extension("md.tmp");
            fs::write(&tmp, &bundled)?;
            fs::rename(&tmp, &target)?;
            refreshed.push(target);
        }
    }
    if !refreshed.is_empty() {
        eprintln!(
            "[agentplug daemon] SKILL.md refreshed: {} target(s)",
            refreshed.len()
        );
    }
    if !failures.is_empty() {
        eprintln!(
            "[agentplug daemon] SKILL.md refresh had {} failure(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(refreshed)
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
        Err(github_api_err) if github_api_err.downcast_ref::<UpdateRejected>().is_some() => Err(github_api_err),
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
                return Ok(dest.to_path_buf());
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

    let artifact = format!("{effective_basename}.wasm");
    let running = installed_plugin_version_from_file(version_file);
    let identity = AssetIdentity {
        artifact: &artifact,
        version: &version,
        running: running.as_deref(),
    };
    snapshot_prev_wasm_and_version(dest, version_file)?;
    let finalized = download_and_verify(&wasm_url, dest, &expected_sha, &identity)?;
    record_plugin_install(dest, version_file, &version, &identity, &finalized)?;
    Ok(dest.to_path_buf())
}
