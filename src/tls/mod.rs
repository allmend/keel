//! Certificate store shared by every TLS listener.
//!
//! Each vhost certificate is parsed twice at load time: into OpenSSL objects
//! for the Pingora proxy listeners (whose per-handshake SNI callback is what
//! makes hot-swap possible there), and into a rustls `CertifiedKey` for
//! rustls-based listeners — the TCP `terminate` mode and HTTP/3, both of
//! which run outside Pingora's OpenSSL data plane. Both views read the same
//! atomically swapped map, so a SIGHUP reload or an ACME issuance replaces
//! the certificate for every listener kind at once.

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
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

// Cert store

struct CertPair {
    cert: X509,
    key: PKey<Private>,
    /// `None` when rustls cannot use the key (OpenSSL accepts more key
    /// types); such a host is served by Pingora listeners only.
    rustls: Option<Arc<CertifiedKey>>,
}

impl CertPair {
    fn from_pem(cert_pem: &[u8], key_pem: &[u8], label: &str) -> anyhow::Result<Self> {
        let cert = X509::from_pem(cert_pem).map_err(|e| anyhow::anyhow!("invalid cert '{label}': {e}"))?;
        let key = PKey::private_key_from_pem(key_pem)
            .map_err(|e| anyhow::anyhow!("invalid key '{label}': {e}"))?;
        let rustls = match certified_key(cert_pem, key_pem) {
            Ok(k) => Some(Arc::new(k)),
            Err(e) => {
                warn!(cert = label, error = %e, "TLS: certificate unusable by rustls listeners");
                None
            }
        };
        Ok(CertPair { cert, key, rustls })
    }
}

/// Parse a PEM chain and key into what rustls serves. The full chain is kept
/// so clients receive intermediates; the leaf is first, as in the file.
fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<CertifiedKey> {
    let chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<_, _>>()?;
    if chain.is_empty() {
        anyhow::bail!("no certificate in PEM");
    }
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut &key_pem[..])?.ok_or_else(|| anyhow::anyhow!("no private key in PEM"))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unsupported key type: {e}"))?;
    Ok(CertifiedKey::new(chain, signing))
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
        Ok(CertStore::from_map(load_cert_map(cfg)?))
    }

    fn from_map(map: CertMap) -> Self {
        CertStore { inner: Arc::new(ArcSwap::from_pointee(map)) }
    }

    /// A store holding one in-memory certificate, for tests of TLS listeners.
    #[cfg(test)]
    pub fn with_test_cert(host: &str, cert_pem: &str, key_pem: &str) -> Self {
        let mut map = HashMap::new();
        map.insert(host.to_owned(), CertPair::from_pem(cert_pem.as_bytes(), key_pem.as_bytes(), host).unwrap());
        CertStore::from_map(map)
    }

    /// Build a boxed `TlsAcceptCallbacks` that reads from this store on every handshake.
    pub fn make_callbacks(&self) -> TlsAcceptCallbacks {
        Box::new(SniCertResolver { certs: Arc::clone(&self.inner) })
    }

    /// The rustls view: a `ResolvesServerCert` that reads this store on every
    /// handshake, so certificates swapped in later are served without
    /// rebuilding the `ServerConfig`.
    pub fn rustls_resolver(&self) -> Arc<RustlsCertResolver> {
        Arc::new(RustlsCertResolver { certs: Arc::clone(&self.inner) })
    }

    /// A rustls `ServerConfig` backed by this store. `alpn` lists the
    /// protocols to offer (for example `["h3"]` or `["http/1.1"]`); empty
    /// offers none. TLS 1.2 is the floor, as on the Pingora listeners.
    #[allow(dead_code)] // the HTTP/3 listener will use SNI-only resolution
    pub fn rustls_server_config(&self, alpn: &[&str]) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        self.build_server_config(alpn, self.rustls_resolver())
    }

    /// Like `rustls_server_config`, but a client that sends no SNI, or one
    /// with no certificate of its own, is served `default_host`'s. TCP
    /// clients (redis-cli, older libpq) often send no SNI at all.
    pub fn rustls_server_config_with_default(
        &self,
        alpn: &[&str],
        default_host: &str,
    ) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        let resolver = Arc::new(DefaultingResolver {
            inner: self.rustls_resolver(),
            default_host: default_host.to_owned(),
        });
        self.build_server_config(alpn, resolver)
    }

    fn build_server_config(
        &self,
        alpn: &[&str],
        resolver: Arc<dyn ResolvesServerCert>,
    ) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut cfg = rustls::ServerConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS12,
            &rustls::version::TLS13,
        ])
        .with_no_client_auth()
        .with_cert_resolver(resolver);
        cfg.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
        Ok(Arc::new(cfg))
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

        let pair = CertPair::from_pem(&cert_bytes, &key_bytes, &cert_path)?;
        info!(vhost = vhost.host, cert = cert_path, "TLS: certificate loaded");
        map.insert(vhost.host.clone(), pair);
    }

    // `certificates:` entries — ACME-issued files or bring-your-own — so
    // terminating TCP listeners can serve them by host name.
    for c in &cfg.certificates {
        let (cert_path, key_path) = match (&c.cert, &c.key) {
            (Some(cert), Some(key)) => (cert.clone(), key.clone()),
            _ => {
                let Some(acme) = acme.as_ref() else { continue };
                let (cert, key) = acme_cert_paths(&acme.storage, &c.host);
                if !std::path::Path::new(&cert).exists() {
                    info!(host = c.host, "TLS: ACME certificate not yet issued; TLS listeners for it wait until issuance");
                    continue;
                }
                (cert, key)
            }
        };
        if map.contains_key(&c.host) {
            continue; // the vhost entry already covers this host
        }
        let cert_bytes = std::fs::read(&cert_path)
            .map_err(|e| anyhow::anyhow!("cannot read cert '{cert_path}': {e}"))?;
        let key_bytes = std::fs::read(&key_path)
            .map_err(|e| anyhow::anyhow!("cannot read key '{key_path}': {e}"))?;
        let pair = CertPair::from_pem(&cert_bytes, &key_bytes, &cert_path)?;
        info!(host = c.host, cert = cert_path, "TLS: certificate loaded");
        map.insert(c.host.clone(), pair);
    }

    Ok(map)
}

/// Client configuration that accepts any backend certificate: wire
/// encryption without backend authentication (NLB behaviour), the default
/// for re-encrypting TCP listeners and for TLS health probes.
pub fn insecure_client_config() -> Arc<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth(),
    )
}

/// Client configuration that verifies backend certificates against the
/// system trust store plus an optional PEM bundle of extra CAs.
pub fn verifying_client_config(extra_ca_pem: Option<&[u8]>) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        warn!(error = %e, "TLS: system trust store entry skipped");
    }
    roots.add_parsable_certificates(native.certs);
    if let Some(pem) = extra_ca_pem {
        let extra: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut &pem[..]).collect::<Result<_, _>>()?;
        if extra.is_empty() {
            anyhow::bail!("tls_ca contains no certificates");
        }
        roots.add_parsable_certificates(extra);
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// Accepts any certificate. Used where the point is encryption and liveness,
/// not identity: re-encryption without `tls_verify`, and health probes.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Exact SNI match, then its one-label wildcard (`*.example.com`), then the
/// `"*"` entry. Shared by both resolvers so the listener kinds never
/// disagree about which certificate a name gets; the same order routing uses.
fn lookup<'a>(map: &'a CertMap, sni: Option<&str>) -> Option<&'a CertPair> {
    let sni = sni.unwrap_or("*");
    map.get(sni)
        .or_else(|| crate::vhost::wildcard_of(sni).and_then(|w| map.get(&w)))
        .or_else(|| map.get("*"))
}

// SNI cert resolver (OpenSSL, Pingora listeners)

/// Selects a certificate per TLS handshake based on the SNI hostname.
/// Falls back to the `"*"` entry if no exact match is found.
struct SniCertResolver {
    certs: Arc<ArcSwap<CertMap>>,
}

#[async_trait]
impl TlsAccept for SniCertResolver {
    async fn certificate_callback(&self, ssl: &mut TlsRef) -> () {
        let sni = ssl.servername(NameType::HOST_NAME).map(str::to_owned);
        let store = self.certs.load();

        match lookup(&store, sni.as_deref()) {
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

// SNI cert resolver (rustls)

/// The same selection for rustls: exact SNI, then `"*"`. Reads the live map
/// on every handshake, so it sees hot-swapped certificates immediately.
pub struct RustlsCertResolver {
    certs: Arc<ArcSwap<CertMap>>,
}

// rustls requires Debug on resolvers; never print key material, only names.
impl std::fmt::Debug for RustlsCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let store = self.certs.load();
        let mut hosts: Vec<&String> = store.keys().collect();
        hosts.sort();
        f.debug_struct("RustlsCertResolver").field("hosts", &hosts).finish()
    }
}

impl RustlsCertResolver {
    /// Resolve by name; `None` when no certificate (or none rustls can use)
    /// matches, which makes rustls abort the handshake.
    pub fn resolve_name(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let store = self.certs.load();
        let pair = lookup(&store, sni)?;
        if pair.rustls.is_none() {
            warn!(sni, "TLS: certificate for SNI hostname is unusable by rustls");
        }
        pair.rustls.clone()
    }
}

impl ResolvesServerCert for RustlsCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_name(client_hello.server_name())
    }
}

/// SNI first, then a listener-specific default host (see
/// `rustls_server_config_with_default`).
#[derive(Debug)]
struct DefaultingResolver {
    inner: Arc<RustlsCertResolver>,
    default_host: String,
}

impl ResolvesServerCert for DefaultingResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.inner
            .resolve_name(client_hello.server_name())
            .or_else(|| self.inner.resolve_name(Some(&self.default_host)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pem_pair(name: &str) -> (Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
    }

    fn store_with(hosts: &[&str]) -> (CertStore, HashMap<String, Vec<u8>>) {
        let mut map = HashMap::new();
        let mut leaves = HashMap::new();
        for h in hosts {
            let (c, k) = pem_pair(if *h == "*" { "fallback.example" } else { h });
            let pair = CertPair::from_pem(&c, &k, h).unwrap();
            leaves.insert(h.to_string(), pair.rustls.as_ref().unwrap().cert[0].to_vec());
            map.insert(h.to_string(), pair);
        }
        (CertStore::from_map(map), leaves)
    }

    #[test]
    fn rustls_view_is_built_from_the_same_pem() {
        let (c, k) = pem_pair("api.example.com");
        let pair = CertPair::from_pem(&c, &k, "test").unwrap();
        let ck = pair.rustls.expect("rustls key");
        assert_eq!(ck.cert.len(), 1);
        assert_eq!(ck.cert[0].as_ref(), pair.cert.to_der().unwrap().as_slice());
    }

    #[test]
    fn resolves_one_label_wildcard_certificates() {
        let (store, leaves) = store_with(&["*.example.com"]);
        let r = store.rustls_resolver();
        let leaf = |sni: Option<&str>| r.resolve_name(sni).map(|k| k.cert[0].to_vec());
        assert_eq!(leaf(Some("www.example.com")), Some(leaves["*.example.com"].clone()));
        assert_eq!(leaf(Some("*.example.com")), Some(leaves["*.example.com"].clone()));
        assert_eq!(leaf(Some("deep.www.example.com")), None);
        assert_eq!(leaf(Some("example.com")), None);
    }

    #[test]
    fn resolves_exact_then_wildcard_then_none() {
        let (store, leaves) = store_with(&["api.example.com", "*"]);
        let r = store.rustls_resolver();
        let leaf = |sni: Option<&str>| r.resolve_name(sni).map(|k| k.cert[0].to_vec());
        assert_eq!(leaf(Some("api.example.com")), Some(leaves["api.example.com"].clone()));
        assert_eq!(leaf(Some("other.example.com")), Some(leaves["*"].clone()));
        assert_eq!(leaf(None), Some(leaves["*"].clone()));

        let (strict, _) = store_with(&["api.example.com"]);
        assert!(strict.rustls_resolver().resolve_name(Some("other.example.com")).is_none());
        assert!(strict.rustls_resolver().resolve_name(None).is_none());
    }

    #[test]
    fn hot_swap_is_visible_without_rebuilding_the_resolver() {
        let (store, before) = store_with(&["api.example.com"]);
        let resolver = store.rustls_resolver();
        assert_eq!(resolver.resolve_name(Some("api.example.com")).unwrap().cert[0].to_vec(), before["api.example.com"]);

        let (c, k) = pem_pair("api.example.com");
        let renewed = CertPair::from_pem(&c, &k, "renewed").unwrap();
        let renewed_leaf = renewed.rustls.as_ref().unwrap().cert[0].to_vec();
        let mut map = HashMap::new();
        map.insert("api.example.com".to_owned(), renewed);
        store.inner.store(Arc::new(map));

        let after = resolver.resolve_name(Some("api.example.com")).unwrap().cert[0].to_vec();
        assert_eq!(after, renewed_leaf);
        assert_ne!(after, before["api.example.com"]);
    }

    #[test]
    fn bad_pem_is_rejected_and_unsupported_keys_only_lose_the_rustls_view() {
        assert!(CertPair::from_pem(b"nope", b"nope", "x").is_err());
        let (c, _) = pem_pair("a.example");
        let (_, other_key) = pem_pair("b.example");
        // OpenSSL parses cert and key independently; rustls builds a key too.
        // Mismatch is caught at handshake time, not here — both views exist.
        let pair = CertPair::from_pem(&c, &other_key, "x").unwrap();
        assert!(pair.rustls.is_some());
    }

    /// End to end: a rustls server on the store's config serves the SNI
    /// certificate, and a swapped certificate is served on the next handshake.
    #[tokio::test]
    async fn serves_sni_certificates_over_a_real_handshake() {
        use rustls::pki_types::ServerName;
        use tokio::net::TcpListener;

        let (store, leaves) = store_with(&["api.example.com", "*"]);
        let server_cfg = store.rustls_server_config(&["http/1.1"]).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(s).await;
                });
            }
        });

        let client_cfg = insecure_client_config();
        let leaf_for = |sni: &'static str| {
            let cfg = Arc::clone(&client_cfg);
            async move {
                let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                let name = ServerName::try_from(sni).unwrap();
                let stream = tokio_rustls::TlsConnector::from(cfg).connect(name, tcp).await.unwrap();
                stream.get_ref().1.peer_certificates().unwrap()[0].to_vec()
            }
        };
        assert_eq!(leaf_for("api.example.com").await, leaves["api.example.com"]);
        assert_eq!(leaf_for("unknown.example.com").await, leaves["*"]);

        // Swap the exact entry; the running acceptor serves the new one.
        let (c, k) = pem_pair("api.example.com");
        let renewed = CertPair::from_pem(&c, &k, "renewed").unwrap();
        let renewed_leaf = renewed.rustls.as_ref().unwrap().cert[0].to_vec();
        let mut map = HashMap::new();
        map.insert("api.example.com".to_owned(), renewed);
        store.inner.store(Arc::new(map));
        assert_eq!(leaf_for("api.example.com").await, renewed_leaf);
    }
}
