fn ensure_crypto_provider_installed() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn extra_trust_anchor_pem_path() -> Option<std::path::PathBuf> {
    std::env::var("AGENTPLUG_EXTRA_CA_CERTS")
        .or_else(|_| std::env::var("SSL_CERT_FILE"))
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
}

fn load_extra_trust_anchors(store: &mut rustls::RootCertStore, pem_path: &std::path::Path) -> anyhow::Result<usize> {
    let file = std::fs::File::open(pem_path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert?;
        if store.add(cert).is_ok() {
            added += 1;
        }
    }
    Ok(added)
}

fn build_root_store() -> rustls::RootCertStore {
    let mut store = rustls::RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem_path) = extra_trust_anchor_pem_path() {
        match load_extra_trust_anchors(&mut store, &pem_path) {
            Ok(added) => eprintln!("[agentplug http] loaded {added} extra trust anchor(s) from {} (AGENTPLUG_EXTRA_CA_CERTS or SSL_CERT_FILE)", pem_path.display()),
            Err(e) => eprintln!("[agentplug http] failed to load extra trust anchors from {}: {e}", pem_path.display()),
        }
    }
    store
}

pub fn build_agent(timeout: std::time::Duration) -> ureq::Agent {
    ensure_crypto_provider_installed();
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(build_root_store())
        .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(std::sync::Arc::new(tls_config))
        .timeout(timeout)
        .build()
}

pub fn shared_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| build_agent(std::time::Duration::from_secs(10)))
}
