use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::hexutil;

pub const STATEMENT_DOMAIN: &str = "agentplug-update-v1";
pub const DOCUMENT_VERSION: u32 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignatureEntry {
    pub key_id: String,
    pub sig: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
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
            return Err(format!("signature document is {} bytes, over the {MAX_DOCUMENT_BYTES} byte limit", text.len()));
        }
        serde_json::from_str::<SignatureDoc>(text).map_err(|e| format!("signature document does not parse: {e}"))
    }

    pub fn statement(&self) -> Result<Vec<u8>, String> {
        statement(&self.artifact, &self.version, self.sequence, &self.sha256)
    }
}

pub fn statement(artifact: &str, version: &str, sequence: u64, sha256: &str) -> Result<Vec<u8>, String> {
    for (label, value) in [("artifact", artifact), ("version", version), ("sha256", sha256)] {
        if value.is_empty() || value.contains(['\n', '\r']) {
            return Err(format!("{label} must be a non-empty single line"));
        }
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
    let Some(sig_bytes) = hexutil::decode_fixed::<64>(signature_hex) else { return false };
    let Ok(key) = VerifyingKey::from_bytes(public) else { return false };
    key.verify_strict(statement, &Signature::from_bytes(&sig_bytes)).is_ok()
}
