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

/// Streams `url` into `dest`, verified against `expected_sha256_hex` before
/// the atomic rename lands -- identical discipline to gm-runner's own
/// download_and_verify (a checksum mismatch never lands a corrupt artifact).
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

/// Every known plugin's release repo -- the mapping from a plugin name (as
/// it appears in a project's `.agentplug/plugins.txt`) to the GitHub repo
/// its `<name>.wasm` + `<name>.wasm.sha256` + `<name>.manifest.json` release
/// assets live in. New plugins register here; this is the one place
/// agentplug-runner needs to know a plugin exists before it can fetch it.
///
/// "gm" is special-cased below (see `PluginAssetSpec`) rather than listed
/// here: it is NOT yet a real agentplug-native plugin release -- there is
/// no AnEntrypoint/agentplug-gm-bin repo. The actual gm wasm (built by
/// rs-plugkit's own long-standing cascade, asset name `plugkit.wasm`, not
/// `gm.wasm`) still ships from AnEntrypoint/plugkit-bin. Routing "gm"
/// through the generic `{name}.wasm` convention here 404s permanently --
/// live-witnessed this session as `plugin gm not loaded` on every dispatch
/// once the daemon tried to auto-serve gm's own spool.
struct PluginAssetSpec {
    repo: &'static str,
    asset_basename: &'static str,
}

/// agentplug-runner always loads "bert" as one of its 4 default plugins
/// (daemon.rs's default plugin list), and agentplug-host's `host_vec_embed`
/// import genuinely routes to that shared bert instance -- so gm.wasm's own
/// `embed.rs::init_ctx()` probe (`probe_host_embed()`) always succeeds under
/// agentplug, meaning gm.wasm's baked-in bert weights (embed.rs's
/// `include_bytes!("weights/bge-small-en-v1.5.safetensors")`, 133MB) are
/// provably dead data: never deserialized into candle tensors, but still
/// copied into the wasm instance's linear memory at Instantiate time as a
/// static data segment. `plugkit-slim.wasm` (same AnEntrypoint/plugkit-bin
/// release, ~3MB, no baked-in weights) is the exact fix -- gm's own JS
/// wrapper (gm-plugkit/bootstrap.js hasNativeEmbedRunner) already applies
/// this same slim-when-a-real-embed-answerer-exists logic; agentplug-runner
/// needs the equivalent since it never routes through that JS bootstrap.
fn gm_asset_basename() -> &'static str {
    "plugkit-slim"
}

fn plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    match plugin_name {
        "gm" => Some(PluginAssetSpec { repo: "AnEntrypoint/plugkit-bin", asset_basename: gm_asset_basename() }),
        "bert" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-bert-bin", asset_basename: "bert" }),
        "libsql" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-libsql-bin", asset_basename: "libsql" }),
        "treesitter" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-treesitter-bin", asset_basename: "treesitter" }),
        _ => None,
    }
}

/// Resolves the LATEST release tag for `repo` WITHOUT touching
/// api.github.com: probes `releases/latest/download/<asset>` (a plain
/// github.com web endpoint, NOT subject to the REST API's unauthenticated
/// 60-requests/hour/IP limit) with redirect-following disabled, and parses
/// the tag out of the first-hop Location header, whose shape is always
/// `https://github.com/<repo>/releases/download/<tag>/<asset>`. The
/// redirect fires even for an asset name that does not exist in the release
/// (the 404 would only happen at the SECOND hop, which is never followed
/// here), so this resolves the tag reliably regardless of asset layout.
///
/// Live-found (plugin-install-rate-limit-lockout): the previous
/// implementation resolved "latest" via an unauthenticated
/// `api.github.com/repos/<repo>/releases/latest` call. Combined with the
/// daemon retrying a failing plugin install on every ~200ms main-loop tick
/// (see PluginModules::get_or_compile's backoff note in daemon.rs), that
/// exhausted the API's 60/hour/IP budget within minutes of boot and then
/// permanently locked out EVERY plugin install on the machine --
/// live-witnessed as `API rate limit exceeded for <ip>` on all three
/// plugin-bin repos while bert/libsql/treesitter sat missing from
/// ~/.agentplug/plugins/, which is what broke embed_text (no bert sibling
/// for host_vec_embed => memorize-fire refused NULL-embedding inserts,
/// recall fell back to LIKE search) and codesearch (no libsql sibling =>
/// libsql_ok=false, fallback_kv mode, zero chunks ever indexed).
fn resolve_latest_tag_via_redirect(repo: &str, probe_asset: &str) -> anyhow::Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest/download/{probe_asset}");
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    // With redirects disabled a 3xx comes back as Ok in current ureq, but
    // guard the Error::Status shape too so a ureq behavior difference never
    // silently drops the Location header this whole function exists to read.
    let location = match agent.get(&url).call() {
        Ok(resp) => resp.header("location").map(|s| s.to_string()),
        Err(ureq::Error::Status(_, resp)) => resp.header("location").map(|s| s.to_string()),
        Err(e) => return Err(e.into()),
    };
    let location = location
        .ok_or_else(|| anyhow::anyhow!("no Location header from {url} -- repo has no releases?"))?;
    parse_tag_from_release_location(&location)
        .ok_or_else(|| anyhow::anyhow!("could not parse release tag from redirect {location} (from {url})"))
}

/// Pure tag extraction from a `releases/download` redirect Location --
/// factored out of resolve_latest_tag_via_redirect for direct unit testing.
fn parse_tag_from_release_location(location: &str) -> Option<String> {
    let marker = "/releases/download/";
    let idx = location.find(marker)?;
    let tag = location[idx + marker.len()..].split('/').next().unwrap_or("");
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

pub fn plugin_wasm_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.wasm"))
}

fn plugin_version_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.version"))
}

/// The runner EXECUTABLE's own self-update, separate from the wasm-guest
/// update path above (that one hot-reloads gm/bert/libsql/treesitter IN this
/// process; this one replaces the process itself). Same source repo and
/// asset-naming convention bin/install.js's agentplugRunnerAssetName() uses
/// (must stay byte-identical to agentplug's own release.yml matrix), so a
/// tag published for the JS installer to pick up is the SAME tag this poll
/// detects -- one release, two independent consumers.
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
    // Redirect-probe instead of api.github.com -- same rate-limit-immunity
    // reasoning as resolve_latest_tag_via_redirect's doc comment; this poll
    // only fires every runner_update_poll_interval (600s default) but it
    // shares the same per-IP API budget the plugin-install loop was
    // exhausting, so it failed alongside them once the limit was hit.
    // Returns the raw tag (e.g. "v0.1.0"), byte-identical to the API's
    // tag_name field this replaced -- record_runner_version / the
    // releases/download/{tag} URL construction both depend on that.
    let Some(asset) = runner_asset_name() else { return Ok(None) };
    Ok(Some(resolve_latest_tag_via_redirect(RUNNER_BIN_REPO, &format!("{asset}.sha256"))?))
}

/// Downloads+verifies a newer runner build to `<current-exe-path>.new`,
/// never overwriting the running exe directly -- Windows refuses to write to
/// its own currently-mapped executable file (a hard OS-level lock, not a
/// permissions issue), so the running process can only ever stage the
/// replacement alongside itself. Returns the staged path and its version tag
/// on success; `Ok(None)` means already current (no newer tag) or this
/// platform has no published runner binary (never an error -- a host CI
/// doesn't build for is expected to silently skip self-update, exactly like
/// the wasm-guest plugin poll's own None-on-unpublished-platform behavior).
pub fn stage_runner_self_update() -> anyhow::Result<Option<(PathBuf, String)>> {
    let Some(asset) = runner_asset_name() else { return Ok(None) };
    let Some(latest) = fetch_latest_runner_version()? else { return Ok(None) };
    if installed_runner_version().as_deref() == Some(latest.as_str()) {
        return Ok(None);
    }
    let current_exe = std::env::current_exe()?;
    let staged = current_exe.with_extension(
        current_exe.extension().map(|e| format!("{}.new", e.to_string_lossy())).unwrap_or_else(|| "new".to_string())
    );
    let base = format!("https://github.com/{RUNNER_BIN_REPO}/releases/download/{latest}");
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

/// Records the version now actually running -- called by the NEW process
/// once it has confirmed itself healthy and taken over ownership (never by
/// the download step itself, which only stages a candidate; writing the
/// version marker before the candidate has proven it can actually serve
/// would let a broken build masquerade as "installed" and never get retried).
pub fn record_runner_version(version: &str) -> anyhow::Result<()> {
    fs::create_dir_all(install_dir())?;
    fs::write(runner_version_path(), version)?;
    Ok(())
}

pub fn fetch_latest_plugin_version(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    // Redirect-probe instead of api.github.com -- see
    // resolve_latest_tag_via_redirect's doc comment for the live-hit
    // rate-limit lockout this replaces. The probe asset name only shapes the
    // redirect's Location string, never a 404 (the redirect fires before any
    // asset-existence check), so the plugkit-slim/plugkit split needs no
    // fallback here -- the tag comes out identical either way.
    let tag = resolve_latest_tag_via_redirect(spec.repo, &format!("{}.wasm.sha256", spec.asset_basename))?;
    Ok(Some(tag.trim_start_matches('v').to_string()))
}

pub fn installed_plugin_version(plugin_name: &str) -> Option<String> {
    fs::read_to_string(plugin_version_path(plugin_name)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn refresh_plugin_if_stale(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(installed) = installed_plugin_version(plugin_name) else {
        return Ok(None);
    };
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

    // plugkit-slim.wasm ships from the same release as plugkit.wasm starting
    // v0.1.915 -- an older/pinned version tag may predate that, so a 404 on
    // the slim sha256 sidecar falls back to the fat asset_basename rather
    // than hard-failing the whole plugin install.
    let sha_url = format!("{base}/{}.wasm.sha256", spec.asset_basename);
    let sha_resp = match ureq::get(&sha_url).call() {
        Ok(resp) => resp,
        Err(_) if spec.asset_basename == "plugkit-slim" => {
            ureq::get(&format!("{base}/plugkit.wasm.sha256")).call()?
        }
        Err(e) => return Err(e.into()),
    };
    let effective_basename = effective_wasm_basename(sha_resp.get_url(), spec.asset_basename);
    let wasm_url = format!("{base}/{effective_basename}.wasm");
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line.split_whitespace().next().ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar for {effective_basename} at {base}"))?.to_string();

    download_and_verify(&wasm_url, &dest, &expected_sha)?;
    fs::write(&version_file, &version)?;
    Ok(dest)
}

/// Which `<basename>.wasm` to download once the sha256 sidecar fetch has
/// resolved: pure decision, extracted for direct unit testing.
///
/// Live-found (plugin-wasm-url-coerced-to-plugkit): this used to read
/// `if sha_final_url.contains("plugkit-slim") { "plugkit-slim" } else
/// { "plugkit" }` -- written for gm's slim-vs-fat fallback but executed for
/// EVERY plugin, so bert/libsql/treesitter (whose sha sidecar fetch had
/// just SUCCEEDED under their own basename) then requested
/// `<their-repo>/releases/download/vX/plugkit.wasm`, a guaranteed 404 --
/// live-witnessed: agentplug-bert-bin/v0.1.0/bert.wasm.sha256 = HTTP 200,
/// same-release plugkit.wasm = HTTP 404. Net effect: the three non-gm
/// default plugins could NEVER install, which is the direct cause of
/// embed_text failing under the native runner (no bert sibling for
/// host_vec_embed -- and plugkit-slim.wasm deliberately carries no baked-in
/// bert weights, so the wasm-side fallback that saved the retired JS host
/// cannot engage either) and of codesearch's permanent libsql_ok=false
/// fallback_kv mode (no libsql sibling). The "plugkit" coercion is only
/// correct for the one plugin whose slim sidecar 404'd and fell back to the
/// fat one; every other plugin keeps its own basename.
fn effective_wasm_basename(sha_final_url: &str, spec_basename: &'static str) -> &'static str {
    if sha_final_url.contains("plugkit-slim") {
        "plugkit-slim"
    } else if spec_basename == "plugkit-slim" {
        "plugkit"
    } else {
        spec_basename
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defect regression: a non-gm plugin whose sha sidecar resolved under
    /// its own name must download ITS OWN wasm, never plugkit.wasm (the
    /// pre-fix coercion that 404'd every bert/libsql/treesitter install).
    #[test]
    fn non_gm_plugin_keeps_its_own_basename() {
        let url = "https://github.com/AnEntrypoint/agentplug-bert-bin/releases/download/v0.1.0/bert.wasm.sha256";
        assert_eq!(effective_wasm_basename(url, "bert"), "bert");
        let url = "https://github.com/AnEntrypoint/agentplug-libsql-bin/releases/download/v0.1.0/libsql.wasm.sha256";
        assert_eq!(effective_wasm_basename(url, "libsql"), "libsql");
        let url = "https://github.com/AnEntrypoint/agentplug-treesitter-bin/releases/download/v0.1.0/treesitter.wasm.sha256";
        assert_eq!(effective_wasm_basename(url, "treesitter"), "treesitter");
    }

    /// gm's slim asset resolves to plugkit-slim -- including when redirects
    /// were followed all the way to the CDN host, where the asset name only
    /// survives inside the querystring (the shape ureq's get_url() actually
    /// returns for a followed release-asset download).
    #[test]
    fn gm_slim_basename_detected_even_in_cdn_querystring_url() {
        let direct = "https://github.com/AnEntrypoint/plugkit-bin/releases/download/v0.1.950/plugkit-slim.wasm.sha256";
        assert_eq!(effective_wasm_basename(direct, "plugkit-slim"), "plugkit-slim");
        let cdn = "https://release-assets.githubusercontent.com/github-production-release-asset/123?rscd=attachment%3B+filename%3Dplugkit-slim.wasm.sha256";
        assert_eq!(effective_wasm_basename(cdn, "plugkit-slim"), "plugkit-slim");
    }

    /// gm's slim sidecar 404'd and the fat plugkit sidecar answered: only
    /// THIS case coerces to plugkit.
    #[test]
    fn gm_fat_fallback_coerces_to_plugkit() {
        let fat = "https://github.com/AnEntrypoint/plugkit-bin/releases/download/v0.1.900/plugkit.wasm.sha256";
        assert_eq!(effective_wasm_basename(fat, "plugkit-slim"), "plugkit");
    }

    /// Tag extraction from the latest/download redirect's first-hop
    /// Location header -- the api.github.com-free version resolution path.
    #[test]
    fn parses_tag_from_release_location() {
        assert_eq!(
            parse_tag_from_release_location(
                "https://github.com/AnEntrypoint/agentplug-bert-bin/releases/download/v0.1.0/bert.wasm.sha256"
            ),
            Some("v0.1.0".to_string())
        );
        assert_eq!(
            parse_tag_from_release_location("https://github.com/x/y/releases/download/v2.3.4-rc1/asset"),
            Some("v2.3.4-rc1".to_string())
        );
        assert_eq!(parse_tag_from_release_location("https://github.com/x/y/releases/latest"), None);
        assert_eq!(parse_tag_from_release_location("https://github.com/x/y/releases/download/"), None);
    }
}
