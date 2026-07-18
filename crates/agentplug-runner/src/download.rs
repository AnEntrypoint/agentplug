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
fn plugin_repo(plugin_name: &str) -> Option<&'static str> {
    match plugin_name {
        "gm" => Some("AnEntrypoint/agentplug-gm-bin"),
        "bert" => Some("AnEntrypoint/agentplug-bert-bin"),
        "libsql" => Some("AnEntrypoint/agentplug-libsql-bin"),
        "treesitter" => Some("AnEntrypoint/agentplug-treesitter-bin"),
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
    let Some(repo) = plugin_repo(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_repo map");
    };
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = ureq::get(&url).set("User-Agent", "agentplug-runner").call()?;
    let body: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
    Ok(body.get("tag_name").and_then(|v| v.as_str()).map(|s| s.trim_start_matches('v').to_string()))
}

pub fn ensure_plugin_installed(plugin_name: &str, explicit_version: Option<&str>) -> anyhow::Result<PathBuf> {
    let dest = plugin_wasm_path(plugin_name);
    if dest.exists() && explicit_version.is_none() {
        return Ok(dest);
    }
    let Some(repo) = plugin_repo(plugin_name) else {
        anyhow::bail!("unknown plugin {plugin_name} -- not registered in agentplug-runner's plugin_repo map");
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

    let base = format!("https://github.com/{repo}/releases/download/v{version}");
    let wasm_url = format!("{base}/{plugin_name}.wasm");
    let sha_url = format!("{base}/{plugin_name}.wasm.sha256");

    let sha_resp = ureq::get(&sha_url).call()?;
    let sha_line = sha_resp.into_string()?;
    let expected_sha = sha_line.split_whitespace().next().ok_or_else(|| anyhow::anyhow!("empty sha256 sidecar at {sha_url}"))?.to_string();

    download_and_verify(&wasm_url, &dest, &expected_sha)?;
    fs::write(&version_file, &version)?;
    Ok(dest)
}
