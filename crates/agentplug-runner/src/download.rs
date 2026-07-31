use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use agentplug_host::install_dir;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn github_api_request(url: &str) -> ureq::Request {
    let req = ureq::get(url).set("User-Agent", "agentplug-runner");
    match std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        Ok(token) if !token.is_empty() => req.set("Authorization", &format!("Bearer {token}")),
        _ => req,
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
        ureq::Error::Status(code, _) => anyhow::anyhow!("GitHub API returned {code} fetching {url}"),
        ureq::Error::Transport(_) => anyhow::Error::from(err).context(format!("network error fetching {url}")),
    }
}

pub fn download_and_verify(url: &str, dest: &Path, expected_sha256_hex: &str) -> anyhow::Result<()> {
    let resp = ureq::get(url).call()?;
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
        anyhow::bail!("sha256 mismatch downloading {url}: expected {expected_sha256_hex}, got {actual}");
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
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
        "gm" => Some(PluginAssetSpec { repo: "AnEntrypoint/plugkit-bin".to_string(), asset_basename: gm_asset_basename().to_string() }),
        "bert" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-bert-bin".to_string(), asset_basename: "bert".to_string() }),
        "libsql" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-libsql-bin".to_string(), asset_basename: "libsql".to_string() }),
        "treesitter" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-treesitter-bin".to_string(), asset_basename: "treesitter".to_string() }),
        _ => None,
    }
}

/// A project-declared extra plugin, read from `.agentplug/plugins.json` in
/// the project root (sibling to `read_project_plugin_list`'s own
/// `.agentplug/plugins.txt`, which already names extra plugins to LOAD but
/// has no way to say where to DOWNLOAD one from). Format: a JSON array of
/// `{"name", "repo", "asset_basename"}` objects, one per extra plugin --
/// `repo` is an `owner/repo` GitHub Releases source, `asset_basename` the
/// asset name prefix (`{base}.wasm`/`{base}.wasm.sha256` at the release tag),
/// mirroring the 4 built-ins' own shape exactly.
#[derive(serde::Deserialize)]
struct ProjectPluginSpec {
    name: String,
    repo: String,
    asset_basename: String,
}

fn project_declared_plugin_specs(project_root: &Path) -> Vec<ProjectPluginSpec> {
    let path = project_root.join(".agentplug").join("plugins.json");
    let Ok(raw) = fs::read_to_string(&path) else { return Vec::new() };
    match serde_json::from_str::<Vec<ProjectPluginSpec>>(&raw) {
        Ok(specs) => specs,
        Err(e) => {
            eprintln!("[agentplug] {} exists but does not parse as an array of {{name,repo,asset_basename}} -- ignoring: {e}", path.display());
            Vec::new()
        }
    }
}

/// Resolve a plugin name to its download spec: a project's own declared spec
/// (from ANY currently-known project root's `.agentplug/plugins.json`) wins
/// over the 4 hardcoded built-ins, so a project can even re-point `gm`/`bert`/
/// `libsql`/`treesitter` at a fork if it explicitly declares one -- otherwise
/// falls through to the compiled-in built-in spec.
fn plugin_asset_spec_for_roots(plugin_name: &str, known_roots: &[PathBuf]) -> Option<PluginAssetSpec> {
    for root in known_roots {
        for spec in project_declared_plugin_specs(root) {
            if spec.name == plugin_name {
                return Some(PluginAssetSpec { repo: spec.repo, asset_basename: spec.asset_basename });
            }
        }
    }
    builtin_plugin_asset_spec(plugin_name)
}

fn plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    let mut roots = crate::daemon::read_known_project_roots();
    // A one-shot `plugin`/`dispatch` CLI invocation never populates the
    // daemon's own known-roots registry (that only fills in via the daemon's
    // own registry-poll loop) -- always also check the current process's own
    // cwd so a single-shot command scoped to one project still honors that
    // project's own .agentplug/plugins.json.
    if let Ok(cwd) = std::env::current_dir() {
        if !roots.contains(&cwd) {
            roots.push(cwd);
        }
    }
    plugin_asset_spec_for_roots(plugin_name, &roots)
}

pub fn plugin_wasm_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.wasm"))
}

fn plugin_version_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.version"))
}

const RUNNER_BIN_REPO: &str = "AnEntrypoint/agentplug-bin";

fn runner_asset_name() -> Option<&'static str> {
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
    fs::read_to_string(runner_version_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn fetch_latest_runner_version() -> anyhow::Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{RUNNER_BIN_REPO}/releases/latest");
    let resp = github_api_request(&url).call().map_err(|e| describe_github_api_error(&url, e))?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    Ok(body.get("tag_name").and_then(|v| v.as_str()).map(|s| s.trim_start_matches('v').to_string()))
}

pub fn stage_runner_self_update() -> anyhow::Result<Option<(PathBuf, String)>> {
    let Some(asset) = runner_asset_name() else { return Ok(None) };
    let Some(latest) = fetch_latest_runner_version()? else { return Ok(None) };
    if marker_is_trustworthy_and_current(latest.as_str()) {
        return Ok(None);
    }
    let mut current_exe = std::env::current_exe()?;
    while current_exe.extension().map(|e| e.eq_ignore_ascii_case("new")).unwrap_or(false) {
        current_exe = current_exe.with_extension("");
    }
    let staged = current_exe.with_extension(
        current_exe.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string())
    );
    let base = format!("https://github.com/{RUNNER_BIN_REPO}/releases/download/v{latest}");
    let sha_line = ureq::get(&format!("{base}/{asset}.sha256")).call()?.into_string()?;
    let expected_sha = sha_line.split_whitespace().next()
        .ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {asset} at {base}"))?.to_string();
    download_and_verify(&format!("{base}/{asset}"), &staged, &expected_sha)?;
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
    let Some(marker) = installed_runner_version() else { return false };
    if marker != latest {
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
    Ok(())
}

pub fn fetch_latest_plugin_version(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let url = format!("https://api.github.com/repos/{}/releases/latest", spec.repo);
    let resp = github_api_request(&url).call().map_err(|e| describe_github_api_error(&url, e))?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    Ok(body.get("tag_name").and_then(|v| v.as_str()).map(|s| s.trim_start_matches('v').to_string()))
}

pub fn installed_plugin_version(plugin_name: &str) -> Option<String> {
    fs::read_to_string(plugin_version_path(plugin_name)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn is_recognized_release_semver(version: &str) -> bool {
    let segments: Vec<&str> = version.split('.').collect();
    segments.len() == 3 && segments.iter().all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

fn local_dev_sideload_marker_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.local-dev-sideload.json"))
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
    fs::read_to_string(local_dev_sideload_marker_path(plugin_name)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn clear_local_dev_sideload_marker(plugin_name: &str) {
    let _ = fs::remove_file(local_dev_sideload_marker_path(plugin_name));
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
    let Some(latest) = fetch_latest_plugin_version(plugin_name)? else {
        return Ok(None);
    };
    if latest == installed {
        return Ok(None);
    }
    ensure_plugin_installed(plugin_name, Some(&latest))?;
    Ok(Some(latest))
}

pub fn ensure_plugin_installed(plugin_name: &str, explicit_version: Option<&str>) -> anyhow::Result<PathBuf> {
    let dest = plugin_wasm_path(plugin_name);
    if dest.exists() && explicit_version.is_none() {
        return Ok(dest);
    }
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let version = match explicit_version {
        Some(v) => v.to_string(),
        None => fetch_latest_plugin_version(plugin_name)?
            .ok_or_else(|| anyhow::anyhow!("could not resolve latest version for plugin {plugin_name}"))?,
    };

    let version_file = plugin_version_path(plugin_name);
    if dest.exists() {
        if let Ok(installed) = fs::read_to_string(&version_file) {
            if installed.trim() == version {
                return Ok(dest);
            }
        }
    }

    let base = format!("https://github.com/{}/releases/download/v{version}", spec.repo);

    let sha_url = format!("{base}/{}.wasm.sha256", spec.asset_basename);
    let sha_resp = match ureq::get(&sha_url).call() {
        Ok(resp) => resp,
        Err(_) if spec.asset_basename == "plugkit-slim" => {
            ureq::get(&format!("{base}/plugkit.wasm.sha256")).call()?
        }
        Err(e) => return Err(e.into()),
    };
    let effective_basename = if sha_resp.get_url().contains("plugkit-slim") { "plugkit-slim" } else { "plugkit" };
    let wasm_url = format!("{base}/{effective_basename}.wasm");
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line.split_whitespace().next().ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {effective_basename} at {base}"))?.to_string();

    let prev_dest = dest.with_extension("wasm.prev");
    if dest.exists() {
        let _ = fs::copy(&dest, &prev_dest);
    }
    download_and_verify(&wasm_url, &dest, &expected_sha)?;
    fs::write(&version_file, &version)?;
    Ok(dest)
}
