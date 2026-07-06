//! Remote control listener — the same control protocol as the Unix socket,
//! over TCP with mandatory mTLS: clients must present a certificate signed
//! by the control CA, and the cert CN identifies the operator in the audit
//! log. `control.remote.allow` optionally restricts accepted source CIDRs;
//! source IPs are unreliable behind NAT / kube-proxy, so the restriction
//! narrows exposure but never replaces mTLS.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ipnet::IpNet;
use rustls_pemfile::{certs, private_key};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::backend::PoolRegistry;
use crate::config::RemoteControlConfig;
use crate::control::ca::ControlCa;

pub struct RemoteControlServer {
    pub cfg: RemoteControlConfig,
    pub pools: Arc<PoolRegistry>,
    pub started_at: Instant,
    pub cluster: Option<crate::cluster::ClusterHandle>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for RemoteControlServer {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        if let Err(e) = self.run(&mut shutdown).await {
            // Refuse to run half-open: an operator who configured remote
            // control must notice it is not serving.
            error!(error = %format!("{e:#}"), "control: remote listener failed");
        }
    }
}

impl RemoteControlServer {
    async fn run(&self, shutdown: &mut pingora::server::ShutdownWatch) -> Result<()> {
        let allow: Vec<IpNet> = self
            .cfg
            .allow
            .iter()
            .map(|c| c.parse().map_err(|e| anyhow::anyhow!("invalid CIDR '{c}': {e}")))
            .collect::<Result<_>>()?;

        let ca = ControlCa::load_or_generate(&self.cfg.ca_dir)?;
        let (server_cert, server_key) = ca.issue_server()?;
        let tls = build_server_tls(&server_cert, &server_key, &ca.ca_cert_pem)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);

        let listener = TcpListener::bind(&self.cfg.address)
            .await
            .with_context(|| format!("bind {}", self.cfg.address))?;
        info!(address = self.cfg.address, allow = ?self.cfg.allow, "control: remote listener ready (mTLS)");

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                result = listener.accept() => {
                    let Ok((stream, peer)) = result else { continue };
                    if !allow.is_empty() && !allow.iter().any(|net| net.contains(&peer.ip())) {
                        warn!(peer = %peer, "control: connection rejected by allow list");
                        continue;
                    }
                    let acceptor = acceptor.clone();
                    let pools = Arc::clone(&self.pools);
                    let started_at = self.started_at;
                    let cluster = self.cluster.clone();
                    tokio::spawn(async move {
                        let tls_stream = match acceptor.accept(stream).await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(peer = %peer, error = %e, "control: TLS handshake failed");
                                return;
                            }
                        };
                        let cn = client_cn(&tls_stream).unwrap_or_else(|| "unknown".into());
                        let audit = format!("{cn}@{peer}");
                        if let Err(e) = crate::control::handle_connection(
                            tls_stream, pools, started_at, cluster, Some(audit),
                        )
                        .await
                        {
                            error!(peer = %peer, error = %e, "control: remote connection error");
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

/// CN of the verified client certificate (the operator name from
/// `keel credentials create <name>`).
fn client_cn(
    stream: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> Option<String> {
    let (_, conn) = stream.get_ref();
    let der = conn.peer_certificates()?.first()?;
    let cert = openssl::x509::X509::from_der(der.as_ref()).ok()?;
    let cn = cert
        .subject_name()
        .entries_by_nid(openssl::nid::Nid::COMMONNAME)
        .next()?
        .data()
        .as_utf8()
        .ok()?;
    Some(cn.to_string())
}

fn build_server_tls(
    cert_pem: &str,
    key_pem: &str,
    ca_pem: &str,
) -> Result<Arc<rustls::ServerConfig>> {
    use std::io::Cursor;
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    for c in certs(&mut Cursor::new(ca_pem.as_bytes())).filter_map(|r| r.ok()) {
        roots.add(c).map_err(|e| anyhow::anyhow!("invalid control CA cert: {e}"))?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| anyhow::anyhow!("client verifier: {e}"))?;

    let cert_chain: Vec<_> =
        certs(&mut Cursor::new(cert_pem.as_bytes())).filter_map(|r| r.ok()).collect();
    let key = private_key(&mut Cursor::new(key_pem.as_bytes()))
        .ok()
        .flatten()
        .context("missing control server key")?;

    let cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)?;
    Ok(Arc::new(cfg))
}
