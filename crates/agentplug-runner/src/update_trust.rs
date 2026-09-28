use agentplug_host::install_dir;
use agentplug_trust::{authorize, commit, hexutil, load_trust, Authorized, Mode, Trust, Verdict};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const EVENTS_FILE_NAME: &str = "update-events.json";
const NOTICE_MARKER_FILE_NAME: &str = "update-trust-notice-shown";
const REPEAT_REPORT_QUIET_MS: u64 = 60 * 60 * 1000;
const VERIFIED_SIDECAR_SUFFIX: &str = "verified";

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
        write!(f, "update rejected: {} {} -- {}", self.artifact, self.version, self.reason)
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn events_path() -> PathBuf {
    install_dir().join(EVENTS_FILE_NAME)
}

fn read_events() -> Events {
    std::fs::read_to_string(events_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_events(events: &Events) {
    let _ = std::fs::create_dir_all(install_dir());
    let _ = std::fs::write(events_path(), serde_json::to_string_pretty(events).unwrap_or_default());
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
    eprintln!("[agentplug UPDATE-TRUST NOTICE] {}", trust_notice_text(trust));
    let _ = std::fs::create_dir_all(install_dir());
    let _ = std::fs::write(marker, now_ms().to_string());
}

fn fetch_signature_text(url: &str) -> Result<String, String> {
    match agentplug_host::shared_agent().get(url).call() {
        Ok(resp) => {
            let limit = agentplug_trust::signature::MAX_DOCUMENT_BYTES as u64;
            let mut text = String::new();
            resp.into_reader().take(limit + 1).read_to_string(&mut text).map_err(|e| format!("reading {url}: {e}"))?;
            Ok(text)
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code} fetching {url}")),
        Err(e) => Err(format!("fetching {url}: {e}")),
    }
}

fn rejection(asset: &AssetIdentity, mode: Mode, reason: String) -> anyhow::Error {
    let mut events = read_events();
    let already_reported = events
        .rejected
        .get(asset.artifact)
        .map(|e| e.version == asset.version && e.reason == reason && now_ms().saturating_sub(e.ts) < REPEAT_REPORT_QUIET_MS)
        .unwrap_or(false);
    if !already_reported {
        eprintln!(
            "[agentplug UPDATE-TRUST REJECTED] refusing {} {} ({} mode): {reason} -- keeping the running version{}",
            asset.artifact,
            asset.version,
            mode.as_str(),
            asset.running.map(|r| format!(" {r}")).unwrap_or_default()
        );
        events.rejected.insert(
            asset.artifact.to_string(),
            Event { version: asset.version.to_string(), reason: reason.clone(), ts: now_ms(), running: asset.running.map(str::to_string) },
        );
        write_events(&events);
    }
    anyhow::Error::new(UpdateRejected { artifact: asset.artifact.to_string(), version: asset.version.to_string(), reason })
}

pub fn preflight(asset: &AssetIdentity, signature_url: &str) -> anyhow::Result<Preflight> {
    let dir = install_dir();
    let trust = load_trust(&dir);
    if trust.mode == Mode::Off {
        return Ok(Preflight { trust, signature: Err("trust mode is off".to_string()) });
    }
    announce_missing_trust_file_once(&trust);
    let signature = fetch_signature_text(signature_url);
    let provisional = match &signature {
        Ok(text) => match agentplug_trust::SignatureDoc::parse(text) {
            Ok(doc) => Some(authorize(&trust, &dir, asset.artifact, asset.version, &doc.sha256, Ok(text))),
            Err(_) => None,
        },
        Err(detail) => Some(authorize(&trust, &dir, asset.artifact, asset.version, "", Err(detail))),
    };
    if let Some(Err(rejected)) = provisional {
        return Err(rejection(asset, rejected.mode, rejected.reason));
    }
    Ok(Preflight { trust, signature })
}

pub fn finalize(asset: &AssetIdentity, pre: &Preflight, bytes: &[u8]) -> anyhow::Result<Finalized> {
    let dir = install_dir();
    let sha256 = hexutil::sha256_hex(bytes);
    let signature = pre.signature.as_ref().map(String::as_str).map_err(String::as_str);
    match authorize(&pre.trust, &dir, asset.artifact, asset.version, &sha256, signature) {
        Ok(authorized) => {
            match &authorized.verdict {
                Verdict::Verified { sequence, signers } => {
                    eprintln!("[agentplug update-trust] verified {} {} sequence {sequence} signed by {signers:?}", asset.artifact, asset.version);
                }
                Verdict::Unverified { reason } => {
                    eprintln!(
                        "[agentplug UPDATE-TRUST WARN] installing UNVERIFIED {} {} (warn mode): {reason}",
                        asset.artifact, asset.version
                    );
                    let mut events = read_events();
                    events.unverified.insert(
                        asset.artifact.to_string(),
                        Event { version: asset.version.to_string(), reason: reason.clone(), ts: now_ms(), running: asset.running.map(str::to_string) },
                    );
                    write_events(&events);
                }
                Verdict::Off => {}
            }
            Ok(Finalized { authorized, sha256 })
        }
        Err(rejected) => Err(rejection(asset, rejected.mode, rejected.reason)),
    }
}

pub fn installed(asset: &AssetIdentity, finalized: &Finalized) {
    let dir = install_dir();
    if let Err(e) = commit(&dir, asset.artifact, &finalized.sha256, &finalized.authorized) {
        eprintln!("[agentplug UPDATE-TRUST WARN] could not record accepted sequence for {}: {e}", asset.artifact);
    }
    if finalized.authorized.verified() {
        let mut events = read_events();
        let changed = events.rejected.remove(asset.artifact).is_some() | events.unverified.remove(asset.artifact).is_some();
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

pub fn mark_staged_verified(staged: &Path, asset: &AssetIdentity) {
    let Ok(bytes) = std::fs::read(staged) else { return };
    let record = serde_json::json!({"artifact": asset.artifact, "version": asset.version, "sha256": hexutil::sha256_hex(&bytes)});
    let _ = std::fs::write(sidecar_path(staged), record.to_string());
}

pub fn staged_runner_permitted(staged: &Path) -> Result<(), String> {
    let trust = load_trust(&install_dir());
    if trust.mode == Mode::Off {
        return Ok(());
    }
    let recorded = std::fs::read_to_string(sidecar_path(staged)).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let recorded_sha = recorded.as_ref().and_then(|v| v.get("sha256")).and_then(|v| v.as_str()).map(str::to_string);
    let actual = std::fs::read(staged).map(|b| hexutil::sha256_hex(&b)).map_err(|e| format!("cannot read staged runner {}: {e}", staged.display()))?;
    let problem = match recorded_sha {
        Some(sha) if sha.eq_ignore_ascii_case(&actual) => return Ok(()),
        Some(sha) => format!("staged runner {} digest {actual} differs from the digest {sha} that passed signature verification", staged.display()),
        None => format!("staged runner {} has no signature-verification record (staged by something other than a verified update)", staged.display()),
    };
    if trust.mode == Mode::Enforce {
        Err(problem)
    } else {
        eprintln!("[agentplug UPDATE-TRUST WARN] proceeding with unverified staged runner (warn mode): {problem}");
        Ok(())
    }
}

pub fn skill_refresh_permitted() -> bool {
    let trust = load_trust(&install_dir());
    match trust.mode {
        Mode::Off => true,
        Mode::Warn => {
            eprintln!("[agentplug UPDATE-TRUST WARN] refreshing SKILL.md from an unsigned remote branch (warn mode): skill text is executable instruction for the agent");
            true
        }
        Mode::Enforce => {
            eprintln!("[agentplug UPDATE-TRUST REJECTED] SKILL.md refresh skipped (enforce mode): the remote skill text is unsigned instruction text");
            false
        }
    }
}

pub fn harden_escalation(marker: &mut serde_json::Value) {
    let trust = load_trust(&install_dir());
    if trust.mode == Mode::Off {
        return;
    }
    marker["command_integrity"] = serde_json::json!("unsigned: this pipes an installer script from a mutable branch and is outside update-signature verification");
    if trust.mode == Mode::Enforce {
        marker["command"] = serde_json::Value::Null;
        marker["manual"] = serde_json::json!(
            "enforce mode withholds the piped installer: download the runner release asset and its .sig, run agentplug-sign verify against your trust directory, then replace the runner yourself"
        );
    }
}

static NOTICE_DELIVERED: AtomicBool = AtomicBool::new(false);

pub fn annotate_instruction(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let trust = load_trust(&install_dir());
    if trust.mode == Mode::Off {
        return false;
    }
    let events = read_events();
    let mut changed = false;
    if !events.rejected.is_empty() {
        let rows: Vec<serde_json::Value> = events
            .rejected
            .iter()
            .map(|(artifact, e)| serde_json::json!({"artifact": artifact, "version": e.version, "reason": e.reason, "keeps_running": e.running, "since_ts": e.ts}))
            .collect();
        obj.insert("update_rejected".to_string(), serde_json::Value::Array(rows));
        changed = true;
    }
    if trust.configured && !events.unverified.is_empty() {
        let names: Vec<String> = events.unverified.iter().map(|(artifact, e)| format!("{artifact}@{}", e.version)).collect();
        obj.insert("update_unverified".to_string(), serde_json::json!(names));
        changed = true;
    }
    if !trust.configured && !NOTICE_DELIVERED.swap(true, Ordering::Relaxed) {
        obj.insert("update_trust_notice".to_string(), serde_json::json!(trust_notice_text(&trust)));
        changed = true;
    }
    changed
}

pub fn status() -> serde_json::Value {
    let dir = install_dir();
    let trust = load_trust(&dir);
    let events = read_events();
    serde_json::json!({
        "trust_file": trust.path.display().to_string(),
        "configured": trust.configured,
        "mode": trust.mode.as_str(),
        "threshold": trust.threshold,
        "keys": trust.keys.iter().map(|k| serde_json::json!({"id": k.id, "public_key": hexutil::encode(&k.public)})).collect::<Vec<_>>(),
        "min_sequence": trust.min_sequence,
        "problem": trust.problem,
        "rejected": events.rejected.keys().collect::<Vec<_>>(),
        "unverified": events.unverified.keys().collect::<Vec<_>>(),
    })
}
