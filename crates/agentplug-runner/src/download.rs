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

fn plugin_asset_spec(plugin_name: &str) -> Option<PluginAssetSpec> {
    match plugin_name {
        "gm" => Some(PluginAssetSpec { repo: "AnEntrypoint/plugkit-bin", asset_basename: "plugkit" }),
        "bert" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-bert-bin", asset_basename: "bert" }),
        "libsql" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-libsql-bin", asset_basename: "libsql" }),
        "treesitter" => Some(PluginAssetSpec { repo: "AnEntrypoint/agentplug-treesitter-bin", asset_basename: "treesitter" }),
        _ => None,
    }
}

pub fn plugin_wasm_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.wasm"))
}

fn plugin_version_path(plugin_name: &str) -> PathBuf {
    install_dir().join("plugins").join(format!("{plugin_name}.version"))
}

pub fn fetch_latest_plugin_version(plugin_name: &str) -> anyhow::Result<Option<String>> {
    let Some(spec) = plugin_asset_spec(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_asset_spec map");
    };
    let url = format!("https://api.github.com/repos/{}/releases/latest", spec.repo);
    let resp = ureq::get(&url).set("User-Agent", "agentplug-runner").call()?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    Ok(body.get("tag_name").and_then(|v| v.as_str()).map(|s| s.trim_start_matches('v').to_string()))
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
    let wasm_url = format!("{base}/{}.wasm", spec.asset_basename);
    let sha_url = format!("{base}/{}.wasm.sha256", spec.asset_basename);

    let sha_resp = ureq::get(&sha_url).call()?;
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line.split_whitespace().next().ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar at {sha_url}"))?.to_string();

    download_and_verify(&wasm_url, &dest, &expected_sha)?;
    fs::write(&version_file, &version)?;
    Ok(dest)
}
