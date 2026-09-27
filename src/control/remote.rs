//! Remote control listener — the same control protocol as the Unix socket,
//! over TCP with mandatory mTLS: clients must present a certificate signed
//! by the control CA, and the cert CN identifies the operator in the audit
//! log. `control.remote.allow` optionally restricts accepted source CIDRs;
//! source IPs are unreliable behind NAT / kube-proxy, so the restriction
//! narrows exposure but never replaces mTLS.
//!
//! The control CA is replicated through the Raft log: the leader publishes
//! its CA once, every node writes it into its own control CA directory and
//! re-keys its listener, so one keelconfig authenticates to all nodes and
//! `keel credentials create` works on any of them.
//!
//! The master binds the port as root and hands it to the control worker,
//! which serves it: no network input reaches the root process.

use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use ipnet::IpNet;
use rustls_pemfile::{certs, private_key};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::config::RemoteControlConfig;
use crate::control::ca::ControlCa;
use crate::control::Dispatch;

/// How long a client has to complete the TLS handshake after connecting.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Connections allowed to be handshaking or served at once. What reaches the
/// listener before authentication cannot spawn tasks without limit.
const MAX_CONNECTIONS: usize = 64;

pub struct RemoteControlServer {
    pub cfg: RemoteControlConfig,
    /// Bound by the master.
    pub listener: std::net::TcpListener,
    /// `control/` under `keel.state_dir`.
    pub ca_dir: String,
    pub dispatch: Arc<Dispatch>,
}

impl RemoteControlServer {
    /// Serve until `shutdown` flips, logging a failed start.
    pub async fn serve(self, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        if let Err(e) = self.run(shutdown).await {
            // Refuse to run half-open: an operator who configured remote
            // control must notice it is not serving.
            error!(error = %format!("{e:#}"), "control: remote listener failed");
        }
    }

    async fn run(self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let allow: Vec<IpNet> = self
            .cfg
            .allow
            .iter()
            .map(|c| c.parse().map_err(|e| anyhow::anyhow!("invalid CIDR '{c}': {e}")))
            .collect::<Result<_>>()?;

        let ca = ControlCa::load_or_generate(&self.ca_dir)?;
        // Swapped when the cluster's CA replaces the local one or a revocation
        // replaces it; each accept reads the current config.
        let tls = Arc::new(ArcSwap::from(server_tls_for(&ca)?));
        tokio::spawn(sync_control_ca(
            self.dispatch.cluster.clone(),
            self.ca_dir.clone(),
            Arc::clone(&tls),
            (ca.ca_cert_pem.clone(), ca.ca_key_pem.clone()),
            shutdown.clone(),
        ));

        self.listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(self.listener).context("serve control.remote")?;
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
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
                    let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                        warn!(peer = %peer, "control: too many remote connections; closed");
                        continue;
                    };
                    let acceptor = tokio_rustls::TlsAcceptor::from(tls.load_full());
                    let dispatch = Arc::clone(&self.dispatch);
                    tokio::spawn(async move {
                        let _slot = slot;
                        // A client that connects and stalls must not hold a
                        // slot indefinitely.
                        let handshake =
                            tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream));
                        let tls_stream = match handshake.await {
                            Ok(Ok(s)) => s,
                            Ok(Err(e)) => {
                                warn!(peer = %peer, error = %e, "control: TLS handshake failed");
                                return;
                            }
                            Err(_) => {
                                warn!(peer = %peer, "control: TLS handshake timed out");
                                return;
                            }
                        };
                        let cn = crate::tls::peer_common_name(tls_stream.get_ref().1).unwrap_or_else(|| "unknown".into());
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
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut rx = cluster.control_ca_rx.clone();
    // Until a CA is committed, followers serve their own: check often, so
    // credentials work on every node within a second of the cluster forming.
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
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
