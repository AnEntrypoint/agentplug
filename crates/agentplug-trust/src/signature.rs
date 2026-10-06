use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::hexutil;

pub const STATEMENT_DOMAIN: &str = "agentplug-update-v1";
pub const DOCUMENT_VERSION: u32 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SignatureEntry {
    pub key_id: String,
    pub sig: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SignatureDoc {
    pub v: u32,
    pub artifact: String,
    pub version: String,
    pub sequence: u64,
    pub sha256: String,
    pub signatures: Vec<SignatureEntry>,
}

impl SignatureDoc {
    pub fn parse(text: &str) -> Result<SignatureDoc, String> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err(format!(
                "signature document is {} bytes, over the {MAX_DOCUMENT_BYTES} byte limit",
                text.len()
            ));
        }
        let doc = serde_json::from_str::<SignatureDoc>(text)
            .map_err(|e| format!("signature document does not parse: {e}"))?;
        doc.validate()?;
        Ok(doc)
    }

    pub fn statement(&self) -> Result<Vec<u8>, String> {
        statement(&self.artifact, &self.version, self.sequence, &self.sha256)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.statement()?;
        if hexutil::decode_fixed::<32>(&self.sha256).is_none() {
            return Err("signature document sha256 is not a 64-hex-digit digest".to_string());
        }
        if self.signatures.is_empty() {
            return Err("signature document has no signatures".to_string());
        }
        let mut key_ids = std::collections::BTreeSet::new();
        for entry in &self.signatures {
            if !is_key_id(&entry.key_id) {
                return Err(format!(
                    "signature key id {:?} must be 1-128 ASCII letters, digits, dots, underscores, or hyphens",
                    entry.key_id
                ));
            }
            if !key_ids.insert(&entry.key_id) {
                return Err(format!(
                    "signature document lists key id {:?} more than once",
                    entry.key_id
                ));
            }
            if hexutil::decode_fixed::<64>(&entry.sig).is_none() {
                return Err(format!(
                    "signature for key id {:?} is not a 128-hex-digit ed25519 signature",
                    entry.key_id
                ));
            }
        }
        Ok(())
    }
}

pub fn is_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn statement(
    artifact: &str,
    version: &str,
    sequence: u64,
    sha256: &str,
) -> Result<Vec<u8>, String> {
    for (label, value) in [
        ("artifact", artifact),
        ("version", version),
        ("sha256", sha256),
    ] {
        if value.is_empty() || value.contains(['\n', '\r']) {
            return Err(format!("{label} must be a non-empty single line"));
        }
    }
    if hexutil::decode_fixed::<32>(sha256).is_none() {
        return Err("sha256 must be a 64-hex-digit digest".to_string());
    }
    Ok(format!(
        "{STATEMENT_DOMAIN}\nartifact:{artifact}\nversion:{version}\nsequence:{sequence}\nsha256:{}\n",
        sha256.to_ascii_lowercase()
    )
    .into_bytes())
}

pub fn public_key_of(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

pub fn sign(secret: &[u8; 32], statement: &[u8]) -> [u8; 64] {
    SigningKey::from_bytes(secret).sign(statement).to_bytes()
}

pub fn verify(public: &[u8; 32], statement: &[u8], signature_hex: &str) -> bool {
    let Some(sig_bytes) = hexutil::decode_fixed::<64>(signature_hex) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    key.verify_strict(statement, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}
