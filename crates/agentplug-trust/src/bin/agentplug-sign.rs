use agentplug_trust::hexutil;
use agentplug_trust::signature::{self, SignatureDoc, SignatureEntry, DOCUMENT_VERSION};
use agentplug_trust::{authorize, commit, load_trust};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const USAGE: &str = "agentplug-sign <command>

  keygen --id <key-id> --out <dir>
      Generate an ed25519 keypair: <dir>/<key-id>.secret (private, hex seed) and <dir>/<key-id>.pub.
      Run on a clean offline device; store the .secret on hardware/offline media. Refuses to write
      inside a git work tree or when running in CI.

  pubkey --key <file.secret>
      Print the public key (hex) of a secret key.

  sign --key <file.secret> [--key <file2.secret> ...] --artifact <file> --version <x.y.z> --sequence <n>
           [--name <published asset name>] [--merge <existing.sig>] [--out <file.sig>]
           [--allow-ci]
      Sign sha256(artifact) + artifact name + version + sequence. --name defaults to the file name.
      --merge adds these signatures to an existing .sig for the same statement (threshold-2 flow).

  verify --artifact <file> --sig <file.sig> --trust-dir <dir> --version <x.y.z> [--name <asset name>] [--record]
      Run the runner's exact acceptance logic against <dir>/trusted-keys.json.
          Exit 0 verified, 2 accepted without verification (off/warn), 1 rejected.";

const CI_SIGNING_AUTHORIZATION_ENV: &str = "AGENTPLUG_RELEASE_SIGNING_CI_AUTHORIZED";

struct Args {
    flags: BTreeMap<String, Vec<String>>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Args, String> {
        let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut iter = raw.iter().peekable();
        while let Some(token) = iter.next() {
            let Some(name) = token.strip_prefix("--") else {
                return Err(format!("unexpected argument {token:?}"));
            };
            let value = match iter.peek() {
                Some(next) if !next.starts_with("--") => iter.next().cloned().unwrap_or_default(),
                _ => String::new(),
            };
            flags.entry(name.to_string()).or_default().push(value);
        }
        Ok(Args { flags })
    }

    fn one(&self, name: &str) -> Result<&str, String> {
        match self.flags.get(name).map(|v| v.as_slice()) {
            Some([value]) if !value.is_empty() => Ok(value),
            Some(_) => Err(format!("--{name} needs exactly one value")),
            None => Err(format!("--{name} is required")),
        }
    }

    fn opt(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn many(&self, name: &str) -> Vec<&str> {
        self.flags
            .get(name)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
}

fn refuse_in_ci() -> Result<(), String> {
    for var in ["GITHUB_ACTIONS", "CI"] {
        if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
            return Err(format!("{var} is set: private keys are never generated or used in CI; run this on a clean offline device"));
        }
    }
    Ok(())
}

fn refuse_signing_in_ci(args: &Args) -> Result<(), String> {
    let protected_release = std::env::var("GITHUB_ACTIONS")
        .map(|value| value == "true")
        .unwrap_or(false)
        && std::env::var(CI_SIGNING_AUTHORIZATION_ENV)
            .map(|value| value == "true")
            .unwrap_or(false);
    if args.has("allow-ci") && protected_release {
        return Ok(());
    }
    if args.has("allow-ci") {
        return Err(
            "--allow-ci requires GitHub Actions and the protected release-signing authorization marker".to_string(),
        );
    }
    refuse_in_ci()
}

fn inside_git_worktree(path: &Path) -> bool {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    absolute.ancestors().any(|dir| {
        let marker = dir.join(".git");
        marker.is_file() || (marker.is_dir() && marker.join("HEAD").exists())
    })
}

fn refuse_inside_repo(path: &Path, what: &str) -> Result<(), String> {
    if inside_git_worktree(path) {
        return Err(format!(
            "{what} {} is inside a git work tree; a private key must never live in a repository",
            path.display()
        ));
    }
    Ok(())
}

fn write_new_private(path: &Path, contents: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn read_secret(path: &str) -> Result<([u8; 32], String), String> {
    refuse_inside_repo(Path::new(path), "secret key file")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let seed = hexutil::decode_fixed::<32>(&text)
        .ok_or_else(|| format!("{path} is not a 64-hex-digit ed25519 seed"))?;
    let id = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".secret").unwrap_or(n).to_string())
        .unwrap_or_default();
    if !signature::is_key_id(&id) {
        return Err(format!(
                "key id {id:?} derived from {path} must be 1-128 ASCII letters, digits, dots, underscores, or hyphens"
            ));
    }
    Ok((seed, id))
}

fn keygen(args: &Args) -> Result<i32, String> {
    refuse_in_ci()?;
    let id = args.one("id")?;
    if !signature::is_key_id(id) {
        return Err(
            "--id must be 1-128 ASCII letters, digits, dots, underscores, or hyphens".to_string(),
        );
    }
    let out = PathBuf::from(args.one("out")?);
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    refuse_inside_repo(&out, "output directory")?;
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| format!("no OS randomness: {e}"))?;
    let public = hexutil::encode(&signature::public_key_of(&seed));
    write_new_private(
        &out.join(format!("{id}.secret")),
        &format!("{}\n", hexutil::encode(&seed)),
    )?;
    std::fs::write(out.join(format!("{id}.pub")), format!("{public}\n"))
        .map_err(|e| format!("cannot write public key: {e}"))?;
    println!(
        "{}",
        serde_json::json!({"id": id, "public_key": public, "secret_file": out.join(format!("{id}.secret")).display().to_string()})
    );
    eprintln!("keep {id}.secret offline (hardware token / encrypted offline media); put only the public key in trusted-keys.json");
    Ok(0)
}

fn pubkey(args: &Args) -> Result<i32, String> {
    let (seed, _) = read_secret(args.one("key")?)?;
    println!("{}", hexutil::encode(&signature::public_key_of(&seed)));
    Ok(0)
}

fn sign(args: &Args) -> Result<i32, String> {
    refuse_signing_in_ci(args)?;
    let artifact_path = PathBuf::from(args.one("artifact")?);
    let version = args.one("version")?.to_string();
    let sequence: u64 = args
        .one("sequence")?
        .parse()
        .map_err(|_| "--sequence must be a non-negative integer".to_string())?;
    let name = match args.opt("name") {
        Some(name) => name.to_string(),
        None => artifact_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("cannot derive an asset name; pass --name")?
            .to_string(),
    };
    let bytes = std::fs::read(&artifact_path)
        .map_err(|e| format!("cannot read {}: {e}", artifact_path.display()))?;
    let sha256 = hexutil::sha256_hex(&bytes);
    let statement = signature::statement(&name, &version, sequence, &sha256)?;
    let mut doc = SignatureDoc {
        v: DOCUMENT_VERSION,
        artifact: name.clone(),
        version,
        sequence,
        sha256,
        signatures: Vec::new(),
    };
    if let Some(existing_path) = args.opt("merge") {
        let existing = SignatureDoc::parse(
            &std::fs::read_to_string(existing_path)
                .map_err(|e| format!("cannot read {existing_path}: {e}"))?,
        )?;
        if existing.artifact != doc.artifact
            || existing.version != doc.version
            || existing.sequence != doc.sequence
            || existing.sha256 != doc.sha256
        {
            return Err("--merge file signs a different statement (artifact/version/sequence/digest differ)".to_string());
        }
        doc.signatures = existing.signatures;
    }
    let keys = args.many("key");
    if keys.is_empty() {
        return Err("--key is required".to_string());
    }
    for key_path in keys {
        let (seed, id) = read_secret(key_path)?;
        if doc.signatures.iter().any(|s| s.key_id == id) {
            continue;
        }
        let sig = signature::sign(&seed, &statement);
        if !signature::verify(
            &signature::public_key_of(&seed),
            &statement,
            &hexutil::encode(&sig),
        ) {
            return Err("self-verification of the fresh signature failed".to_string());
        }
        doc.signatures.push(SignatureEntry {
            key_id: id,
            sig: hexutil::encode(&sig),
        });
    }
    doc.validate()?;
    let out = args
        .opt("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{}.sig", artifact_path.display())));
    std::fs::write(
        &out,
        format!(
            "{}\n",
            serde_json::to_string(&doc).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    println!(
        "{}",
        serde_json::json!({"signed": name, "sha256": doc.sha256, "sequence": sequence, "signers": doc.signatures.iter().map(|s| &s.key_id).collect::<Vec<_>>(), "out": out.display().to_string()})
    );
    Ok(0)
}

fn verify(args: &Args) -> Result<i32, String> {
    let artifact_path = PathBuf::from(args.one("artifact")?);
    let version = args.one("version")?.to_string();
    let dir = PathBuf::from(args.one("trust-dir")?);
    let name = match args.opt("name") {
        Some(name) => name.to_string(),
        None => artifact_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("cannot derive an asset name; pass --name")?
            .to_string(),
    };
    let bytes = std::fs::read(&artifact_path)
        .map_err(|e| format!("cannot read {}: {e}", artifact_path.display()))?;
    let sha256 = hexutil::sha256_hex(&bytes);
    let sig_text = args
        .opt("sig")
        .map(|p| std::fs::read_to_string(p).map_err(|e| format!("cannot read {p}: {e}")));
    let trust = load_trust(&dir);
    let sig_arg = match &sig_text {
        Some(Ok(text)) => Ok(text.as_str()),
        Some(Err(e)) => Err(e.as_str()),
        None => Err("no --sig given"),
    };
    match authorize(&trust, &dir, &name, &version, &sha256, sig_arg) {
        Ok(authorized) => {
            let (label, code) = match &authorized.verdict {
                agentplug_trust::Verdict::Verified { .. } => ("verified", 0),
                agentplug_trust::Verdict::Unverified { .. } => {
                    ("unverified-installed-under-warn", 2)
                }
                agentplug_trust::Verdict::Off => ("off", 2),
            };
            if args.has("record") {
                commit(&dir, &name, &sha256, &authorized)
                    .map_err(|e| format!("cannot record sequence: {e}"))?;
            }
            println!(
                "{}",
                serde_json::json!({"verdict": label, "mode": authorized.mode.as_str(), "trust_configured": authorized.configured, "detail": format!("{:?}", authorized.verdict), "sha256": sha256})
            );
            Ok(code)
        }
        Err(rejected) => {
            println!(
                "{}",
                serde_json::json!({"verdict": "rejected", "mode": rejected.mode.as_str(), "reason": rejected.reason, "sha256": sha256})
            );
            Ok(1)
        }
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = raw.split_first() else {
        eprintln!("{USAGE}");
        std::process::exit(64);
    };
    let result = Args::parse(rest).and_then(|args| match command.as_str() {
        "keygen" => keygen(&args),
        "pubkey" => pubkey(&args),
        "sign" => sign(&args),
        "verify" => verify(&args),
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    });
    match result {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("agentplug-sign: {message}");
            std::process::exit(64);
        }
    }
}
