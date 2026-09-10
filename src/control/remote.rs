//! Remote control listener — the same control protocol as the Unix socket,
//! over TCP with mandatory mTLS: clients must present a certificate signed
//! by the control CA, and the cert CN identifies the operator in the audit
//! log. `control.remote.allow` optionally restricts accepted source CIDRs;
//! source IPs are unreliable behind NAT / kube-proxy, so the restriction
//! narrows exposure but never replaces mTLS.
//!
//! In cluster mode the control CA is replicated through the Raft log: the
//! leader publishes its CA once, every node writes it into its own `ca_dir`
//! and re-keys its listener, so one keelconfig authenticates to all nodes
//! and `keel credentials create` works on any of them.

use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use ipnet::IpNet;
use rustls_pemfile::{certs, private_key};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::RemoteControlConfig;
use crate::control::ca::ControlCa;
use crate::control::Dispatch;

pub struct RemoteControlServer {
    pub cfg: RemoteControlConfig,
    pub dispatch: Arc<Dispatch>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for RemoteControlServer {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        self.serve(&mut shutdown).await
    }
}

impl RemoteControlServer {
    /// Serve until `shutdown` flips, logging a failed start. Split out of the
    /// `BackgroundService` impl so the master can run the listener on its own
    /// runtime, without a Pingora server.
    pub async fn serve(&self, shutdown: &mut pingora::server::ShutdownWatch) {
        if let Err(e) = self.run(shutdown).await {
            // Refuse to run half-open: an operator who configured remote
            // control must notice it is not serving.
            error!(error = %format!("{e:#}"), "control: remote listener failed");
        }
    }

    async fn run(&self, shutdown: &mut pingora::server::ShutdownWatch) -> Result<()> {
        let allow: Vec<IpNet> = self
            .cfg
            .allow
            .iter()
            .map(|c| c.parse().map_err(|e| anyhow::anyhow!("invalid CIDR '{c}': {e}")))
            .collect::<Result<_>>()?;

        let ca = ControlCa::load_or_generate(&self.cfg.ca_dir)?;
        // Swapped when the cluster's CA replaces the local one; each accept
        // reads the current config.
        let tls = Arc::new(ArcSwap::from(server_tls_for(&ca)?));
        if let Some(cluster) = self.dispatch.cluster() {
            tokio::spawn(sync_control_ca(
                cluster,
                self.cfg.ca_dir.clone(),
                Arc::clone(&tls),
                (ca.ca_cert_pem.clone(), ca.ca_key_pem.clone()),
                shutdown.clone(),
            ));
        }

        // Parsed, not resolved: tokio only binds a literal address inline, and
        // routes anything needing name resolution through spawn_blocking. That
        // would leave a blocking-pool thread alive in the master, and the
        // master's fork() for a replacement worker must happen from a
        // single-threaded process (see run_master).
        let address: std::net::SocketAddr = self
            .cfg
            .address
            .parse()
            .with_context(|| format!("control.remote.address must be ip:port, got '{}'", self.cfg.address))?;
        let listener =
            TcpListener::bind(address).await.with_context(|| format!("bind {address}"))?;
        crate::control::register_master_listener(std::os::fd::AsRawFd::as_raw_fd(&listener));
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
                    let acceptor = tokio_rustls::TlsAcceptor::from(tls.load_full());
                    let dispatch = Arc::clone(&self.dispatch);
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
                        if let Err(e) =
                            crate::control::handle_connection(tls_stream, dispatch, Some(audit))
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

/// Server TLS for the listener: a fresh server certificate from `ca`, and
/// `ca` as the only trusted client issuer.
fn server_tls_for(ca: &ControlCa) -> Result<Arc<rustls::ServerConfig>> {
    let (server_cert, server_key) = ca.issue_server()?;
    build_server_tls(&server_cert, &server_key, &ca.ca_cert_pem)
}

/// Keep this node's control CA equal to the cluster's. The leader publishes
/// its local CA when the log holds none; every node adopts what the log
/// holds, replacing its local files and re-keying the listener. Raft is the
/// source of truth once a CA is committed: two nodes that started with
/// different local CAs converge on the leader's.
async fn sync_control_ca(
    cluster: crate::cluster::ClusterHandle,
    ca_dir: String,
    tls: Arc<ArcSwap<rustls::ServerConfig>>,
    mut local: (String, String),
    mut shutdown: pingora::server::ShutdownWatch,
) {
    let mut rx = cluster.control_ca_rx.clone();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        let replicated = rx.borrow_and_update().clone();
        match replicated {
            Some((cert_pem, key_pem)) if cert_pem != local.0 => {
                match ControlCa::install(&ca_dir, &cert_pem, &key_pem).and_then(|ca| server_tls_for(&ca)) {
                    Ok(cfg) => {
                        tls.store(cfg);
                        info!(
                            ca_dir,
                            "control: adopted the cluster control CA; credentials issued by \
                             this node's previous CA no longer authenticate"
                        );
                        local = (cert_pem, key_pem);
                    }
                    Err(e) => error!(error = %format!("{e:#}"), "control: cannot install cluster control CA"),
                }
            }
            Some(_) => {}
            None => {
                // Nothing committed yet: only the leader publishes, so the
                // cluster converges on one CA instead of racing.
                if let Some(raft) = cluster.raft().await {
                    let m = raft.metrics().borrow().clone();
                    if m.current_leader == Some(m.id) {
                        match crate::cluster::push_control_ca(&raft, local.0.clone(), local.1.clone()).await {
                            Ok(()) => info!("control: published this node's control CA to the cluster"),
                            Err(e) => warn!(error = %e, "control: could not publish control CA"),
                        }
                    }
                }
            }
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() { return; }
            }
            _ = tick.tick() => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
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
