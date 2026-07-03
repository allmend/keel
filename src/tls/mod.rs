use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::listeners::{TlsAccept, TlsAcceptCallbacks};
use pingora::protocols::tls::TlsRef;
use pingora::tls::{
    ext::{ssl_use_certificate, ssl_use_private_key},
    pkey::{PKey, Private},
    ssl::NameType,
    x509::X509,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

// Cert store

struct CertPair {
    cert: X509,
    key: PKey<Private>,
}

type CertMap = HashMap<String, CertPair>;

/// Holds parsed TLS certificates for all vhosts, swappable atomically on hot reload.
pub struct CertStore {
    inner: Arc<ArcSwap<CertMap>>,
}

impl CertStore {
    /// Load all vhost TLS certificates from the paths in `cfg`.
    /// Returns an error if any cert or key file cannot be read or parsed.
    pub fn build(cfg: &crate::config::Config) -> anyhow::Result<Self> {
        Ok(CertStore {
            inner: Arc::new(ArcSwap::from_pointee(load_cert_map(cfg)?)),
        })
    }

    /// Build a boxed `TlsAcceptCallbacks` that reads from this store on every handshake.
    pub fn make_callbacks(&self) -> TlsAcceptCallbacks {
        Box::new(SniCertResolver { certs: Arc::clone(&self.inner) })
    }

    /// Re-read all cert/key files from the new config and atomically swap them in.
    /// On failure, keeps the previous certificates and returns the error.
    pub fn reload(&self, cfg: &crate::config::Config) -> anyhow::Result<()> {
        let map = load_cert_map(cfg)?;
        self.inner.store(Arc::new(map));
        Ok(())
    }
}

/// Cert/key file paths for an ACME-managed host inside the storage directory.
pub fn acme_cert_paths(storage: &str, host: &str) -> (String, String) {
    (format!("{storage}/{host}.crt"), format!("{storage}/{host}.key"))
}

fn load_cert_map(cfg: &crate::config::Config) -> anyhow::Result<CertMap> {
    let mut map = HashMap::new();
    let acme = cfg.acme_effective();

    for vhost in &cfg.vhosts {
        let Some(tls_cfg) = &vhost.tls else { continue };

        let (cert_path, key_path) = if tls_cfg.acme.enabled() {
            let storage = &acme.as_ref().expect("validated: acme vhost implies acme config").storage;
            let (cert, key) = acme_cert_paths(storage, &vhost.host);
            // Before first issuance the files don't exist — start without a cert
            // for this host; AcmeService hot-swaps it in once issued.
            if !std::path::Path::new(&cert).exists() {
                info!(vhost = vhost.host, "TLS: ACME certificate not yet issued; serving without it until issuance");
                continue;
            }
            (cert, key)
        } else {
            (
                tls_cfg.cert.clone().expect("validated: cert required without acme"),
                tls_cfg.key.clone().expect("validated: key required without acme"),
            )
        };

        let cert_bytes = std::fs::read(&cert_path)
            .map_err(|e| anyhow::anyhow!("cannot read cert '{cert_path}': {e}"))?;
        let key_bytes = std::fs::read(&key_path)
            .map_err(|e| anyhow::anyhow!("cannot read key '{key_path}': {e}"))?;

        let cert = X509::from_pem(&cert_bytes)
            .map_err(|e| anyhow::anyhow!("invalid cert '{cert_path}': {e}"))?;
        let key = PKey::private_key_from_pem(&key_bytes)
            .map_err(|e| anyhow::anyhow!("invalid key '{key_path}': {e}"))?;

        info!(vhost = vhost.host, cert = cert_path, "TLS: certificate loaded");
        map.insert(vhost.host.clone(), CertPair { cert, key });
    }

    Ok(map)
}

// SNI cert resolver

/// Selects a certificate per TLS handshake based on the SNI hostname.
/// Falls back to the `"*"` entry if no exact match is found.
struct SniCertResolver {
    certs: Arc<ArcSwap<CertMap>>,
}

#[async_trait]
impl TlsAccept for SniCertResolver {
    async fn certificate_callback(&self, ssl: &mut TlsRef) -> () {
        let sni = ssl.servername(NameType::HOST_NAME).unwrap_or("*").to_owned();
        let store = self.certs.load();
        let pair = store.get(&sni).or_else(|| store.get("*"));

        match pair {
            Some(pair) => {
                if let Err(e) = ssl_use_certificate(ssl, &pair.cert) {
                    error!(sni, error = %e, "TLS: failed to set certificate");
                    return;
                }
                if let Err(e) = ssl_use_private_key(ssl, &pair.key) {
                    error!(sni, error = %e, "TLS: failed to set private key");
                }
            }
            None => warn!(sni, "TLS: no certificate found for SNI hostname"),
        }
    }
}
