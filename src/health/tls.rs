//! TLS handshake health probe.
//!
//! Completes a TLS handshake with the backend and, with `min_days_valid`,
//! checks how long the presented certificate stays valid. The chain is not
//! verified: the probe asks "does this backend speak TLS and is its
//! certificate about to expire", not "do I trust it" — that is the client's
//! business in passthrough mode. rustls does the handshake (pure Rust, same
//! stack as the cluster mesh); OpenSSL, already linked for the proxy
//! listeners, reads the certificate dates.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;

use super::{io_reason, Probe, ProbeResult};

pub struct TlsProbe {
    config: Arc<rustls::ClientConfig>,
    sni: Option<String>,
    min_days_valid: Option<u32>,
}

impl TlsProbe {
    pub fn new(sni: Option<String>, min_days_valid: Option<u32>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        TlsProbe { config: Arc::new(config), sni, min_days_valid }
    }
}

#[async_trait]
impl Probe for TlsProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        let tcp = TcpStream::connect(target).await.map_err(|e| io_reason(&e))?;
        let name: ServerName<'static> = match &self.sni {
            Some(s) => ServerName::try_from(s.clone()).map_err(|_| format!("invalid sni {s:?}"))?,
            None => ServerName::IpAddress(target.ip().into()),
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.config));
        let stream = connector.connect(name, tcp).await.map_err(|e| format!("handshake: {e}"))?;
        let Some(min_days) = self.min_days_valid else { return Ok(()) };
        let (_, conn) = stream.get_ref();
        let leaf = conn
            .peer_certificates()
            .and_then(|chain| chain.first())
            .ok_or_else(|| "no certificate presented".to_owned())?;
        check_expiry(leaf.as_ref(), min_days)
    }
}

/// `Err` when the DER certificate is expired or expires within `min_days`.
pub fn check_expiry(der: &[u8], min_days: u32) -> ProbeResult {
    let cert = openssl::x509::X509::from_der(der).map_err(|e| format!("certificate: {e}"))?;
    let now = openssl::asn1::Asn1Time::days_from_now(0).map_err(|e| e.to_string())?;
    let diff = now.diff(cert.not_after()).map_err(|e| e.to_string())?;
    let secs = i64::from(diff.days) * 86_400 + i64::from(diff.secs);
    if secs < 0 {
        return Err(format!("certificate expired {} days ago", -secs / 86_400));
    }
    let days = secs / 86_400;
    if days < i64::from(min_days) {
        return Err(format!("certificate expires in {days} days"));
    }
    Ok(())
}

/// Accepts any certificate: the probe checks liveness and expiry, not trust.
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::TcpListener;

    /// Self-signed cert for localhost; `expired` dates it in the past.
    fn self_signed(expired: bool) -> (Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        if expired {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        }
        let cert = params.self_signed(&key).unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    async fn tls_server(expired: bool) -> SocketAddr {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (cert, key) = self_signed(expired);
        let cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(stream).await;
                });
            }
        });
        addr
    }

    #[test]
    fn expiry_arithmetic() {
        let (fresh, _) = self_signed(false);
        assert_eq!(check_expiry(&fresh, 30), Ok(()));
        assert!(check_expiry(&fresh, u32::MAX).unwrap_err().starts_with("certificate expires in "));
        let (old, _) = self_signed(true);
        assert!(check_expiry(&old, 1).unwrap_err().starts_with("certificate expired "));
        assert!(check_expiry(b"junk", 1).unwrap_err().starts_with("certificate: "));
    }

    #[tokio::test]
    async fn handshake_and_expiry_against_a_local_server() {
        let fresh = tls_server(false).await;
        assert_eq!(TlsProbe::new(None, None).probe(fresh).await, Ok(()));
        assert_eq!(TlsProbe::new(Some("localhost".into()), Some(30)).probe(fresh).await, Ok(()));

        let expired = tls_server(true).await;
        assert_eq!(TlsProbe::new(None, None).probe(expired).await, Ok(()), "handshake alone passes");
        let reason = TlsProbe::new(None, Some(1)).probe(expired).await.unwrap_err();
        assert!(reason.starts_with("certificate expired "), "{reason}");

        // A plain TCP listener is not a TLS server.
        let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = plain.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = plain.accept().await.unwrap();
                let _ = tokio::io::AsyncWriteExt::write_all(&mut s, b"HTTP/1.0 200 OK\r\n\r\n").await;
            }
        });
        let reason = TlsProbe::new(None, None).probe(addr).await.unwrap_err();
        assert!(reason.starts_with("handshake: "), "{reason}");
    }
}
