use agentplug_host::install_dir;
use agentplug_trust::{authorize, commit, hexutil, load_trust, Authorized, Mode, Trust, Verdict};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const REQUIRE_SIGNATURE_ENV: &str = "AGENTPLUG_REQUIRE_RUNNER_SIGNATURE";
const RUNNER_ARTIFACT_PREFIX: &str = "agentplug-runner";
const EVENTS_FILE_NAME: &str = "update-events.json";
const UNVERIFIED_PROMOTION_FILE_NAME: &str = "runner-unverified-update.json";
const VERIFIED_SIDECAR_SUFFIX: &str = "verified";
const NOTICE_MARKER_FILE_NAME: &str = "update-trust-notice-shown";

pub struct AssetIdentity<'a> {
    pub artifact: &'a str,
    pub version: &'a str,
    pub running: Option<&'a str>,
}

#[derive(Debug)]
pub struct UpdateRejected {
    pub artifact: String,
    pub version: String,
    pub reason: String,
}

impl std::fmt::Display for UpdateRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "update rejected: {} {} -- {}",
            self.artifact, self.version, self.reason
        )
    }
}

impl std::error::Error for UpdateRejected {}

pub struct Preflight {
    trust: Trust,
    signature: Result<String, String>,
}

pub struct Finalized {
    pub authorized: Authorized,
    pub sha256: String,
    signature: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Event {
    version: String,
    reason: String,
    ts: u64,
    running: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Events {
    #[serde(default)]
    rejected: BTreeMap<String, Event>,
    #[serde(default)]
    unverified: BTreeMap<String, Event>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn events_path() -> PathBuf {
    install_dir().join(EVENTS_FILE_NAME)
}

fn read_events() -> Events {
    fs::read_to_string(events_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_events(events: &Events) {
    let _ = fs::create_dir_all(install_dir());
    let _ = fs::write(
        events_path(),
        serde_json::to_string_pretty(events).unwrap_or_default(),
    );
}

pub fn unverified_promotion_path() -> PathBuf {
    install_dir().join(UNVERIFIED_PROMOTION_FILE_NAME)
}

pub fn strict_mode_source() -> Option<String> {
    if crate::download::env_flag_enabled(REQUIRE_SIGNATURE_ENV) {
        return Some(format!("{REQUIRE_SIGNATURE_ENV} is set"));
    }
    if crate::daemon::daemon_requires_runner_signature() {
        return Some("require_runner_signature in daemon-config.json".to_string());
    }
    None
}

pub fn strict_mode() -> bool {
    strict_mode_source().is_some()
}

fn is_runner_artifact(artifact: &str) -> bool {
    artifact.starts_with(RUNNER_ARTIFACT_PREFIX)
}

fn is_first_party_plugin_artifact(artifact: &str) -> bool {
    matches!(artifact, "plugkit-slim.wasm" | "bert.wasm" | "crux.wasm")
}

fn embedded_runner_trust(path: PathBuf) -> Trust {
    let (id, public_key) = crate::build_info::embedded_runner_root();
    let public = hexutil::decode_fixed::<32>(public_key)
        .expect("the committed runner release root must be a 64-hex-digit ed25519 public key");
    Trust {
        path,
        configured: true,
        mode: Mode::Enforce,
        threshold: 1,
        keys: vec![agentplug_trust::trust_file::TrustedKey {
            id: id.to_string(),
            public,
        }],
        min_sequence: BTreeMap::new(),
        problem: None,
    }
}

fn embedded_plugin_trust(path: PathBuf) -> Trust {
    let (id, public_key) = crate::build_info::embedded_plugin_root();
    let public = hexutil::decode_fixed::<32>(public_key)
        .expect("the committed plugin release root must be a 64-hex-digit ed25519 public key");
    Trust {
        path,
        configured: true,
        mode: Mode::Enforce,
        threshold: 1,
        keys: vec![agentplug_trust::trust_file::TrustedKey {
            id: id.to_string(),
            public,
        }],
        min_sequence: BTreeMap::new(),
        problem: None,
    }
}

fn trust_for(artifact: &str) -> Trust {
    let mut trust = load_trust(&install_dir());
    if !trust.configured {
        trust = if is_runner_artifact(artifact) {
            embedded_runner_trust(trust.path)
        } else if is_first_party_plugin_artifact(artifact) {
            embedded_plugin_trust(trust.path)
        } else {
            trust
        };
    }
    if trust.mode != Mode::Enforce
        && (is_runner_artifact(artifact) || is_first_party_plugin_artifact(artifact))
        && (!trust.configured || strict_mode())
    {
        trust.mode = Mode::Enforce;
    }
    trust
}

fn runner_trust() -> Trust {
    trust_for(RUNNER_ARTIFACT_PREFIX)
}

pub fn runner_mode_str() -> &'static str {
    runner_trust().mode.as_str()
}

fn trust_notice_text(trust: &Trust) -> String {
    format!(
        "update signing is NOT enforced: no trust file at {}, so runner and plugin updates install after a sha256 check only (warn mode, nothing is verified). Generate an ed25519 key pair on a clean offline device with agentplug-sign keygen, pin the public key in that file, then set mode to enforce -- see docs/SIGNED-UPDATES.md",
        trust.path.display()
    )
}

fn announce_missing_trust_file_once(trust: &Trust) {
    if trust.configured {
        return;
    }
    let marker = install_dir().join(NOTICE_MARKER_FILE_NAME);
    if marker.exists() {
        return;
    }
    eprintln!(
        "[agentplug UPDATE-TRUST NOTICE] {}",
        trust_notice_text(trust)
    );
    let _ = fs::create_dir_all(install_dir());
    let _ = fs::write(marker, now_ms().to_string());
}

fn fetch_signature_text(url: &str) -> Result<String, String> {
    match agentplug_host::shared_agent().get(url).call() {
        Ok(resp) => {
            let limit = agentplug_trust::signature::MAX_DOCUMENT_BYTES as u64;
            let mut text = String::new();
            resp.into_reader()
                .take(limit + 1)
                .read_to_string(&mut text)
                .map_err(|e| format!("reading {url}: {e}"))?;
            Ok(text)
        }
        Err(ureq::Error::Status(404, _)) => {
            Err(format!("no signature published at {url} (HTTP 404)"))
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code} fetching {url}")),
        Err(e) => Err(format!("fetching {url}: {e}")),
    }
}

fn rejection(asset: &AssetIdentity, mode: Mode, reason: String) -> anyhow::Error {
    let reason = match strict_mode_source() {
        Some(source) if is_runner_artifact(asset.artifact) => {
            format!("{reason} (strict mode demanded a signature: {source})")
        }
        _ => reason,
    };
    let mut events = read_events();
    let already_recorded = events
        .rejected
        .get(asset.artifact)
        .map(|e| e.version == asset.version && e.reason == reason)
        .unwrap_or(false);
    if !already_recorded {
        eprintln!(
            "[agentplug UPDATE-TRUST REJECTED] refusing {} {} ({} mode): {reason} -- keeping the running version{}",
            asset.artifact,
            asset.version,
            mode.as_str(),
            asset.running.map(|r| format!(" {r}")).unwrap_or_default()
        );
        events.rejected.insert(
            asset.artifact.to_string(),
            Event {
                version: asset.version.to_string(),
                reason: reason.clone(),
                ts: now_ms(),
                running: asset.running.map(str::to_string),
            },
        );
        write_events(&events);
    }
    anyhow::Error::new(UpdateRejected {
        artifact: asset.artifact.to_string(),
        version: asset.version.to_string(),
        reason,
    })
}

pub fn preflight(asset: &AssetIdentity, signature_url: &str) -> anyhow::Result<Preflight> {
    let trust = trust_for(asset.artifact);
    if trust.mode == Mode::Off {
        return Ok(Preflight {
            trust,
            signature: Err("trust mode is off".to_string()),
        });
    }
    announce_missing_trust_file_once(&trust);
    let signature = fetch_signature_text(signature_url);
    let provisional = match &signature {
        Ok(text) => match agentplug_trust::SignatureDoc::parse(text) {
            Ok(doc) => Some(authorize(
                &trust,
                &install_dir(),
                asset.artifact,
                asset.version,
                &doc.sha256,
                Ok(text),
            )),
            Err(_) => None,
        },
        Err(detail) => Some(authorize(
            &trust,
            &install_dir(),
            asset.artifact,
            asset.version,
            "",
            Err(detail),
        )),
    };
    if let Some(Err(rejected)) = provisional {
        return Err(rejection(asset, rejected.mode, rejected.reason));
    }
    Ok(Preflight { trust, signature })
}

pub fn finalize(asset: &AssetIdentity, pre: &Preflight, bytes: &[u8]) -> anyhow::Result<Finalized> {
    let sha256 = hexutil::sha256_hex(bytes);
    let signature = pre
        .signature
        .as_ref()
        .map(String::as_str)
        .map_err(String::as_str);
    match authorize(
        &pre.trust,
        &install_dir(),
        asset.artifact,
        asset.version,
        &sha256,
        signature,
    ) {
        Ok(authorized) => {
            if let Verdict::Unverified { reason } = &authorized.verdict {
                let mut events = read_events();
                let already_recorded = events
                    .unverified
                    .get(asset.artifact)
                    .map(|e| e.version == asset.version && e.reason == *reason)
                    .unwrap_or(false);
                if !already_recorded {
                    eprintln!(
                        "[agentplug UPDATE-TRUST WARN] installing UNVERIFIED {} {} (release tag v{}, {} mode): {reason} -- only the sha256 sidecar from the same channel proves these bytes; see {}",
                        asset.artifact,
                        asset.version,
                        asset.version,
                        authorized.mode.as_str(),
                        events_path().display()
                    );
                    events.unverified.insert(
                        asset.artifact.to_string(),
                        Event {
                            version: asset.version.to_string(),
                            reason: reason.clone(),
                            ts: now_ms(),
                            running: asset.running.map(str::to_string),
                        },
                    );
                    write_events(&events);
                }
            }
            Ok(Finalized {
                authorized,
                sha256,
                signature: pre.signature.clone().ok(),
            })
        }
        Err(rejected) => Err(rejection(asset, rejected.mode, rejected.reason)),
    }
}

pub fn installed(asset: &AssetIdentity, finalized: &Finalized) {
    if let Err(e) = commit(
        &install_dir(),
        asset.artifact,
        &finalized.sha256,
        &finalized.authorized,
    ) {
        eprintln!(
            "[agentplug UPDATE-TRUST WARN] could not record accepted sequence for {}: {e}",
            asset.artifact
        );
    }
    if finalized.authorized.verified() {
        let mut events = read_events();
        let changed = events.rejected.remove(asset.artifact).is_some()
            | events.unverified.remove(asset.artifact).is_some();
        if changed {
            write_events(&events);
        }
    }
}

fn sidecar_path(staged: &Path) -> PathBuf {
    let mut name = staged.as_os_str().to_os_string();
    name.push(format!(".{VERIFIED_SIDECAR_SUFFIX}"));
    PathBuf::from(name)
}

pub fn record_stage_outcome(staged: &Path, asset: &AssetIdentity, finalized: &Finalized) {
    let reason = match &finalized.authorized.verdict {
        Verdict::Verified { .. } => None,
        Verdict::Unverified { reason } => Some(reason.clone()),
        Verdict::Off => Some("update-signature verification is off for this install".to_string()),
    };
    let record = serde_json::json!({
        "artifact": asset.artifact,
        "version": asset.version,
        "sha256": finalized.sha256,
        "verified": reason.is_none(),
        "reason": reason,
        "signature": finalized.signature,
        "ts": now_ms(),
    });
    let _ = fs::write(sidecar_path(staged), record.to_string());
}

fn read_stage_record(staged: &Path) -> Option<serde_json::Value> {
    fs::read_to_string(sidecar_path(staged))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub fn remove_stage_record(staged: &Path) {
    let _ = fs::remove_file(sidecar_path(staged));
}

pub fn staged_runner_permitted(
    staged: &Path,
    expected_artifact: &str,
    expected_version: &str,
) -> Result<bool, String> {
    let trust = runner_trust();
    if trust.mode == Mode::Off {
        return Ok(false);
    }
    if !trust.configured {
        return Err(format!(
            "no trust file at {}; automatic runner promotion requires a verified signature",
            trust.path.display()
        ));
    }
    let actual = fs::read(staged)
        .map(|bytes| hexutil::sha256_hex(&bytes))
        .map_err(|e| format!("cannot read staged runner {}: {e}", staged.display()))?;
    let record = read_stage_record(staged);
    let recorded_sha = record
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let recorded_artifact = record
        .as_ref()
        .and_then(|v| v.get("artifact"))
        .and_then(|v| v.as_str());
    let recorded_version = record
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str());
    if recorded_artifact != Some(expected_artifact) {
        return Err(format!(
            "the staged runner verification record is for artifact {:?}, not {expected_artifact:?}",
            recorded_artifact
        ));
    }
    if recorded_version != Some(expected_version) {
        return Err(format!(
            "the staged runner verification record is for version {:?}, not {expected_version:?}",
            recorded_version
        ));
    }
    let signature = record
        .as_ref()
        .and_then(|v| v.get("signature"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(|| {
            fetch_signature_text(&crate::download::runner_signature_url(
                expected_artifact,
                expected_version,
            ))
        });
    match recorded_sha {
        Some(sha) if sha.eq_ignore_ascii_case(&actual) => {
            match authorize(
                &trust,
                &install_dir(),
                expected_artifact,
                expected_version,
                &actual,
                signature.as_deref().map_err(String::as_str),
            ) {
                Ok(authorized) if authorized.verified() => {
                    commit(&install_dir(), expected_artifact, &actual, &authorized)
                        .map_err(|error| format!("could not record promoted runner sequence: {error}"))?;
                    Ok(true)
                }
                Ok(_) => Ok(false),
                Err(rejected) => Err(rejected.reason),
            }
        }
        Some(sha) => Err(format!(
            "staged runner {} digest {actual} differs from the digest {sha} recorded when it was staged",
            staged.display()
        )),
        None => Err("the staged runner carries no verification record".to_string()),
    }
}

pub fn record_unverified_promotion(staged: &Path, version: &str, running: Option<&str>) {
    let path = unverified_promotion_path();
    let record = read_stage_record(staged);
    let verified = record
        .as_ref()
        .and_then(|v| v.get("verified"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if verified || runner_trust().mode == Mode::Off {
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        return;
    }
    let reason = record
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            "the promoted runner carried no signature-verification record".to_string()
        });
    let sha256 = record
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let artifact = record
        .as_ref()
        .and_then(|v| v.get("artifact"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let marker = serde_json::json!({
        "artifact": artifact,
        "version": version,
        "release_tag": format!("v{version}"),
        "sha256": sha256,
        "reason": reason,
        "promoted_at_ts": now_ms(),
        "running_before": running,
    });
    let already = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let unchanged = already
        .as_ref()
        .map(|old| {
            old.get("version").and_then(|v| v.as_str()) == Some(version)
                && old.get("reason").and_then(|v| v.as_str()) == Some(reason.as_str())
        })
        .unwrap_or(false);
    let _ = fs::create_dir_all(install_dir());
    let _ = fs::write(&path, marker.to_string());
    if !unchanged {
        eprintln!(
            "[agentplug UPDATE-TRUST WARN] promoted UNVERIFIED runner {} (release tag v{version}) -- {reason}; recorded at {}",
            artifact,
            path.display()
        );
    }
}

pub fn unverified_promotion() -> Option<serde_json::Value> {
    fs::read_to_string(unverified_promotion_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub fn status() -> serde_json::Value {
    let dir = install_dir();
    let trust = load_trust(&dir);
    let runner_trust = runner_trust();
    let (embedded_id, embedded_public_key) = crate::build_info::embedded_runner_root();
    let runner_uses_embedded_root = !trust.configured;
    let events = read_events();
    serde_json::json!({
        "trust_file": trust.path.display().to_string(),
        "configured": trust.configured,
        "mode": trust.mode.as_str(),
        "runner_mode": runner_trust.mode.as_str(),
        "runner_signature_required": strict_mode(),
        "runner_signature_required_by": strict_mode_source(),
        "runner_trust_source": if runner_uses_embedded_root { "embedded-release-root" } else { "trusted-keys.json" },
        "embedded_runner_root": {"id": embedded_id, "public_key": embedded_public_key},
        "runner_trust": {
            "configured": runner_trust.configured,
            "threshold": runner_trust.threshold,
            "keys": runner_trust.keys.iter().map(|k| serde_json::json!({"id": k.id, "public_key": hexutil::encode(&k.public)})).collect::<Vec<_>>(),
        },
        "threshold": trust.threshold,
        "keys": trust.keys.iter().map(|k| serde_json::json!({"id": k.id, "public_key": hexutil::encode(&k.public)})).collect::<Vec<_>>(),
        "min_sequence": trust.min_sequence,
        "problem": trust.problem,
        "rejected": events.rejected,
        "unverified": events.unverified,
        "unverified_promotion": unverified_promotion(),
    })
}
