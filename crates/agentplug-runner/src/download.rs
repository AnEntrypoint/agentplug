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

fn npm_registry_latest_version(package: &str) -> anyhow::Result<String> {
    let url = format!("https://registry.npmjs.org/{package}/latest");
    let resp = agentplug_host::shared_agent().get(&url).call()?;
    let body = resp.into_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    v.get("version")
        .and_then(|s| s.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("npm registry response for {package} had no version field"))
}

fn npm_registry_tarball_url(package: &str, version: &str) -> anyhow::Result<String> {
    let url = format!("https://registry.npmjs.org/{package}/{version}");
    let resp = agentplug_host::shared_agent().get(&url).call()?;
    let body = resp.into_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    v.get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("npm registry response for {package}@{version} had no dist.tarball field"))
}

fn extract_file_from_npm_tarball(tarball_bytes: &[u8], file_name: &str) -> anyhow::Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(tarball_bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path.ends_with(&format!("/{file_name}")) || path == file_name {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    anyhow::bail!("npm tarball did not contain a file named {file_name} (package.json's own \"main\" convention -- expected package/{file_name})")
}

fn try_ensure_plugin_installed_via_npm_mirror(spec: &PluginAssetSpec, dest: &Path, version_file: &Path) -> anyhow::Result<PathBuf> {
    let package = spec.npm_package.as_deref().ok_or_else(|| anyhow::anyhow!("no npm mirror configured for this plugin"))?;
    let version = npm_registry_latest_version(package)?;
    let tarball_url = npm_registry_tarball_url(package, &version)?;
    let tarball_resp = agentplug_host::shared_agent().get(&tarball_url).call()?;
    let mut tarball_bytes = Vec::new();
    tarball_resp.into_reader().read_to_end(&mut tarball_bytes)?;

    let wasm_name = format!("{}.wasm", spec.asset_basename);
    let sha_name = format!("{}.wasm.sha256", spec.asset_basename);
    let (effective_basename, wasm_bytes, sha_bytes) = match extract_file_from_npm_tarball(&tarball_bytes, &wasm_name) {
        Ok(wasm_bytes) => {
            let sha_bytes = extract_file_from_npm_tarball(&tarball_bytes, &sha_name)?;
            (spec.asset_basename.as_str(), wasm_bytes, sha_bytes)
        }
        Err(slim_err) if spec.asset_basename == "plugkit-slim" => {
            let wasm_bytes = extract_file_from_npm_tarball(&tarball_bytes, "plugkit.wasm")
                .map_err(|_| slim_err)?;
            let sha_bytes = extract_file_from_npm_tarball(&tarball_bytes, "plugkit.wasm.sha256")?;
            ("plugkit", wasm_bytes, sha_bytes)
        }
        Err(e) => return Err(e),
    };
    let sha_text = String::from_utf8_lossy(&sha_bytes).into_owned();
    let expected_sha = sha_text.split_whitespace().next()
        .ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {effective_basename} in npm package {package}"))?
        .to_string();
    let actual_sha = sha256_hex(&wasm_bytes);
    if actual_sha != expected_sha {
        anyhow::bail!("npm mirror {package}@{version} sha256 mismatch: expected {expected_sha}, got {actual_sha}");
    }

    let prev_dest = dest.with_extension("wasm.prev");
    if dest.exists() {
        let _ = fs::copy(dest, &prev_dest);
    }
    let tmp = dest.with_extension("wasm.tmp");
    fs::write(&tmp, &wasm_bytes)?;
    fs::rename(&tmp, dest)?;
    fs::write(version_file, &version)?;
    eprintln!("[agentplug] {effective_basename} installed via npm mirror {package}@{version} (GitHub Releases path failed or was unreachable)");
    Ok(dest.to_path_buf())
}

fn github_api_request(url: &str) -> ureq::Request {
    let req = agentplug_host::shared_agent().get(url).set("User-Agent", "agentplug-runner");
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
    npm_package: Option<String>,
}

fn gm_asset_basename() -> &'static str {
    "plugkit-slim"
}

fn builtin_plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    match plugin_name {
        "gm" => Some(PluginAssetSpec { repo: "AnEntrypoint/plugkit-bin".to_string(), asset_basename: gm_asset_basename().to_string(), npm_package: Some("plugkit-wasm".to_string()) }),
        "bert" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-bert-bin".to_string(), asset_basename: "bert".to_string(), npm_package: Some("agentplug-bert-wasm".to_string()) }),
        "libsql" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-libsql-bin".to_string(), asset_basename: "libsql".to_string(), npm_package: Some("agentplug-libsql-wasm".to_string()) }),
        "treesitter" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-treesitter-bin".to_string(), asset_basename: "treesitter".to_string(), npm_package: Some("agentplug-treesitter-wasm".to_string()) }),
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
                return Some(PluginAssetSpec { repo: spec.repo, asset_basename: spec.asset_basename, npm_package: None });
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
    let running_from_staged_path = std::env::current_exe()?
        .extension()
        .map(|e| e.eq_ignore_ascii_case("new"))
        .unwrap_or(false);
    let mut current_exe = std::env::current_exe()?;
    while current_exe.extension().map(|e| e.eq_ignore_ascii_case("new")).unwrap_or(false) {
        current_exe = current_exe.with_extension("");
    }
    // A process that is ITSELF still executing from a `.new`-suffixed path
    // (its own promote_staged_exe_to_canonical copied its bytes onto the
    // canonical path, but a running process's OWN loaded executable image
    // stays locked to whatever file it was launched from -- Windows refuses
    // to rename/overwrite the backing file of a live process's mapped image
    // with "Access is denied", and that lock never clears for this
    // process's whole lifetime) must stage the NEXT update under a
    // DIFFERENT filename than `.new`, or every subsequent self-update
    // attempt would try to rename onto the exact path this process is
    // itself locked onto and fail identically forever. `.new2` sidesteps
    // that collision; a process legitimately running from the canonical
    // path (the common case) is unaffected and keeps using `.new` as before.
    let staged_suffix = if running_from_staged_path { "new2" } else { "new" };
    let staged = current_exe.with_extension(
        current_exe.extension().map(|e| format!("{}.{staged_suffix}", e.to_string_lossy())).unwrap_or_else(|| staged_suffix.to_string())
    );
    let base = format!("https://github.com/{RUNNER_BIN_REPO}/releases/download/v{latest}");
    let sha_line = agentplug_host::shared_agent().get(&format!("{base}/{asset}.sha256")).call()?.into_string()?;
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

fn fetch_remote_wasm_sha256(plugin_name: &str, version: &str) -> anyhow::Result<String> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let base = format!("https://github.com/{}/releases/download/v{version}", spec.repo);
    let sha_line = agentplug_host::shared_agent().get(&format!("{base}/{}.wasm.sha256", spec.asset_basename)).call()?.into_string()?;
    sha_line.split_whitespace().next().map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {} at {base}", spec.asset_basename))
}

fn installed_wasm_sha256(plugin_name: &str) -> Option<String> {
    fs::read(plugin_wasm_path(plugin_name)).ok().map(|bytes| sha256_hex(&bytes))
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
        match (fetch_remote_wasm_sha256(plugin_name, &latest), installed_wasm_sha256(plugin_name)) {
            (Ok(remote_sha), Some(local_sha)) if !remote_sha.eq_ignore_ascii_case(&local_sha) => {
                eprintln!("[agentplug daemon] plugin {plugin_name} version {latest} matches but the released asset's sha256 has changed ({remote_sha} vs installed {local_sha}) -- re-fetching under the same tag");
            }
            _ => return Ok(None),
        }
    }
    ensure_plugin_installed(plugin_name, Some(&latest))?;
    if plugin_name == "gm" {
        if let Err(e) = refresh_installed_skill_md() {
            eprintln!("[agentplug daemon] SKILL.md refresh after gm plugin update to {latest} failed: {e:#}");
        }
    }
    Ok(Some(latest))
}

const SKILL_MD_REMOTE_REPO: &str = "AnEntrypoint/gm";
const SKILL_MD_REMOTE_BRANCH: &str = "main";

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn installed_skill_roots(home: &Path) -> [PathBuf; 2] {
    [home.join(".agents").join("skills"), home.join(".claude").join("skills")]
}

fn discover_installed_skill_names(home: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = std::collections::BTreeSet::new();
    for root in installed_skill_roots(home) {
        let Ok(entries) = fs::read_dir(&root) else { continue };
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
    Ok(agentplug_host::shared_agent().get(&url).call()?.into_string()?)
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
                Ok(existing) => sha256_hex(normalize_newlines(&existing).as_bytes()) != bundled_hash,
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
        eprintln!("[agentplug daemon] SKILL.md refreshed: {} target(s)", refreshed.len());
    }
    if !failures.is_empty() {
        eprintln!("[agentplug daemon] SKILL.md refresh had {} failure(s): {}", failures.len(), failures.join("; "));
    }
    Ok(refreshed)
}

pub fn ensure_plugin_installed(plugin_name: &str, explicit_version: Option<&str>) -> anyhow::Result<PathBuf> {
    let dest = plugin_wasm_path(plugin_name);
    if dest.exists() && explicit_version.is_none() {
        return Ok(dest);
    }
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let version_file = plugin_version_path(plugin_name);

    match ensure_plugin_installed_via_github(plugin_name, explicit_version, &spec, &dest, &version_file) {
        Ok(path) => Ok(path),
        Err(github_err) => {
            match try_ensure_plugin_installed_via_npm_mirror(&spec, &dest, &version_file) {
                Ok(path) => Ok(path),
                Err(npm_err) => Err(anyhow::anyhow!(
                    "plugin {plugin_name} install failed on both paths -- GitHub Releases: {github_err:#}; npm mirror: {npm_err:#}"
                )),
            }
        }
    }
}

fn ensure_plugin_installed_via_github(plugin_name: &str, explicit_version: Option<&str>, spec: &PluginAssetSpec, dest: &Path, version_file: &Path) -> anyhow::Result<PathBuf> {
    let version = match explicit_version {
        Some(v) => v.to_string(),
        None => fetch_latest_plugin_version(plugin_name)?
            .ok_or_else(|| anyhow::anyhow!("could not resolve latest version for plugin {plugin_name}"))?,
    };

    if dest.exists() {
        if let Ok(installed) = fs::read_to_string(version_file) {
            if installed.trim() == version {
                return Ok(dest.to_path_buf());
            }
        }
    }

    let base = format!("https://github.com/{}/releases/download/v{version}", spec.repo);

    let sha_url = format!("{base}/{}.wasm.sha256", spec.asset_basename);
    let mut effective_basename = spec.asset_basename.as_str();
    let sha_resp = match agentplug_host::shared_agent().get(&sha_url).call() {
        Ok(resp) => resp,
        Err(_) if spec.asset_basename == "plugkit-slim" => {
            effective_basename = "plugkit";
            agentplug_host::shared_agent().get(&format!("{base}/plugkit.wasm.sha256")).call()?
        }
        Err(e) => return Err(e.into()),
    };
    let wasm_url = format!("{base}/{effective_basename}.wasm");
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line.split_whitespace().next().ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {effective_basename} at {base}"))?.to_string();

    let prev_dest = dest.with_extension("wasm.prev");
    if dest.exists() {
        let _ = fs::copy(dest, &prev_dest);
    }
    download_and_verify(&wasm_url, dest, &expected_sha)?;
    fs::write(version_file, &version)?;
    Ok(dest.to_path_buf())
}
