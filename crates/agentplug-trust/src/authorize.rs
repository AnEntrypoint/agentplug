use std::collections::BTreeSet;
use std::path::Path;

use crate::sequence;
use crate::signature::{self, SignatureDoc, DOCUMENT_VERSION};
use crate::trust_file::{Mode, Trust};

#[derive(Clone, Debug)]
pub enum Verdict {
    Off,
    Verified { sequence: u64, signers: Vec<String> },
    Unverified { reason: String },
}

#[derive(Clone, Debug)]
pub struct Authorized {
    pub mode: Mode,
    pub configured: bool,
    pub verdict: Verdict,
}

impl Authorized {
    pub fn verified(&self) -> bool {
        matches!(self.verdict, Verdict::Verified { .. })
    }
}

#[derive(Clone, Debug)]
pub struct Rejected {
    pub mode: Mode,
    pub reason: String,
}

pub struct Proven {
    pub sequence: u64,
    pub signers: Vec<String>,
}

pub fn prove(
    trust: &Trust,
    dir: &Path,
    artifact: &str,
    version: &str,
    sha256: &str,
    doc_text: &str,
) -> Result<Proven, String> {
    if let Some(problem) = &trust.problem {
        return Err(format!("trust file unusable, failing closed: {problem}"));
    }
    let doc = SignatureDoc::parse(doc_text)?;
    if doc.v != DOCUMENT_VERSION {
        return Err(format!(
            "signature document version {} is not the supported version {DOCUMENT_VERSION}",
            doc.v
        ));
    }
    if doc.artifact != artifact {
        return Err(format!(
            "signature is for artifact {:?}, not {artifact:?}",
            doc.artifact
        ));
    }
    if doc.version != version {
        return Err(format!(
            "signature is for version {:?}, but version {version:?} is being installed",
            doc.version
        ));
    }
    if !doc.sha256.eq_ignore_ascii_case(sha256) {
        return Err(format!(
            "signed digest {} does not match the downloaded bytes {sha256}",
            doc.sha256
        ));
    }
    let statement = doc.statement()?;
    let mut signer_public: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut signers: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for entry in &doc.signatures {
        let Some(key) = trust.key(&entry.key_id) else {
            unknown.push(entry.key_id.clone());
            continue;
        };
        if signature::verify(&key.public, &statement, &entry.sig)
            && signer_public.insert(key.public)
        {
            signers.push(key.id.clone());
        }
    }
    if signers.len() < trust.threshold {
        return Err(match (signers.is_empty(), unknown.is_empty()) {
            (true, false) if unknown.len() == doc.signatures.len() => {
                format!(
                    "signed only by key(s) {unknown:?}, none of which are pinned in {}",
                    trust.path.display()
                )
            }
            (true, _) => "no signature verifies against the pinned keys".to_string(),
            _ => format!(
                "{} valid pinned signature(s), {} required",
                signers.len(),
                trust.threshold
            ),
        });
    }
    let recorded = sequence::accepted(dir, artifact);
    let floor = recorded
        .as_ref()
        .map(|a| a.sequence)
        .unwrap_or(0)
        .max(trust.min_sequence.get(artifact).copied().unwrap_or(0));
    if doc.sequence < floor {
        return Err(format!(
            "rollback: sequence {} is below the accepted floor {floor} for {artifact}",
            doc.sequence
        ));
    }
    if let Some(recorded) = &recorded {
        if recorded.sequence == doc.sequence && !recorded.sha256.eq_ignore_ascii_case(sha256) {
            return Err(format!(
                "sequence {} was already accepted for {artifact} with a different digest",
                doc.sequence
            ));
        }
    }
    Ok(Proven {
        sequence: doc.sequence,
        signers,
    })
}

pub fn authorize(
    trust: &Trust,
    dir: &Path,
    artifact: &str,
    version: &str,
    sha256: &str,
    signature_doc: Result<&str, &str>,
) -> Result<Authorized, Rejected> {
    if trust.mode == Mode::Off {
        return Ok(Authorized {
            mode: Mode::Off,
            configured: trust.configured,
            verdict: Verdict::Off,
        });
    }
    let outcome = match (&trust.problem, signature_doc) {
        (Some(problem), _) => Err(format!("trust file unusable, failing closed: {problem}")),
        (None, Ok(text)) => prove(trust, dir, artifact, version, sha256, text),
        (None, Err(detail)) => Err(format!("no signature available: {detail}")),
    };
    match outcome {
        Ok(proven) => Ok(Authorized {
            mode: trust.mode,
            configured: trust.configured,
            verdict: Verdict::Verified {
                sequence: proven.sequence,
                signers: proven.signers,
            },
        }),
        Err(reason) if trust.mode == Mode::Warn => Ok(Authorized {
            mode: Mode::Warn,
            configured: trust.configured,
            verdict: Verdict::Unverified { reason },
        }),
        Err(reason) => Err(Rejected {
            mode: trust.mode,
            reason,
        }),
    }
}

pub fn commit(
    dir: &Path,
    artifact: &str,
    sha256: &str,
    authorized: &Authorized,
) -> std::io::Result<()> {
    match &authorized.verdict {
        Verdict::Verified { sequence, .. } => sequence::record(dir, artifact, *sequence, sha256),
        _ => Ok(()),
    }
}
