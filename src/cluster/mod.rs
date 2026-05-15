pub mod network;
pub mod store;
pub mod types;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use openraft::{BasicNode, Config as RaftConfig, Raft};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, private_key};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use crate::cluster::network::{ClusterNetworkFactory, RpcRequest, RpcResponse};
use crate::cluster::store::{LogStore, StateMachine};
use crate::cluster::types::{ClientRequest, NodeId, TypeConfig};

pub type ClusterRaft = Raft<TypeConfig>;

// Options

pub struct ClusterOpts {
    pub node_id: NodeId,
    pub cluster_addr: String,
    pub secret: Option<String>,
    pub bootstrap: bool,
    pub join: Option<String>,
}

// Cluster handle

#[derive(Clone)]
pub struct ClusterHandle {
    /// Becomes Some once ClusterService::start() completes Raft initialization.
    pub raft: Arc<Mutex<Option<Arc<ClusterRaft>>>>,
    /// Fires when a new config YAML is committed to the Raft log.
    pub config_rx: watch::Receiver<Option<String>>,
}

impl ClusterHandle {
    pub async fn raft(&self) -> Option<Arc<ClusterRaft>> {
        self.raft.lock().await.clone()
    }
}

// Certificate helpers

struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
}

impl Ca {
    fn generate() -> Result<Self> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(rcgen::DnType::CommonName, "Keel Cluster CA");
        let cert = params.self_signed(&key)?;
        let cert_pem = cert.pem();
        let issuer = Issuer::new(params, key);
        Ok(Self { issuer, cert_pem })
    }

    fn issue_node_cert(&self, node_id: NodeId) -> Result<(String, String)> {
        let node_key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::NoCa;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("keel-node-{node_id}"));
        params.subject_alt_names = vec![rcgen::SanType::DnsName(
            "keel-cluster".try_into().map_err(|e| anyhow::anyhow!("SAN: {e:?}"))?,
        )];
        let node_cert = params.signed_by(&node_key, &self.issuer)?;
        Ok((node_cert.pem(), node_key.serialize_pem()))
    }
}

fn parse_certs(pem: &str) -> Vec<CertificateDer<'static>> {
    certs(&mut Cursor::new(pem.as_bytes())).filter_map(Result::ok).collect()
}

fn parse_key(pem: &str) -> Option<PrivateKeyDer<'static>> {
    private_key(&mut Cursor::new(pem.as_bytes())).ok().flatten()
}

fn build_client_tls(
    node_cert_pem: &str,
    node_key_pem: &str,
    ca_cert_pem: &str,
) -> Result<Arc<rustls::ClientConfig>> {
    // Install ring as the process-default crypto provider (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    for c in parse_certs(ca_cert_pem) {
        root_store.add(c).map_err(|e| anyhow::anyhow!("invalid CA cert: {e}"))?;
    }

    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(
            parse_certs(node_cert_pem),
            parse_key(node_key_pem).context("missing node private key")?,
        )?;

    Ok(Arc::new(cfg))
}

fn build_server_tls(
    node_cert_pem: &str,
    node_key_pem: &str,
    ca_cert_pem: &str,
) -> Result<Arc<rustls::ServerConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    for c in parse_certs(ca_cert_pem) {
        root_store.add(c).map_err(|e| anyhow::anyhow!("invalid CA cert: {e}"))?;
    }

    let verifier =
        rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store)).build()?;

    let cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            parse_certs(node_cert_pem),
            parse_key(node_key_pem).context("missing node private key")?,
        )?;

    Ok(Arc::new(cfg))
}

// Join protocol

#[derive(Serialize, Deserialize, Debug)]
struct JoinRequest {
    secret: String,
    node_id: NodeId,
    addr: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct JoinResponse {
    ca_cert_pem: String,
    node_cert_pem: String,
    node_key_pem: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JoinEnvelope {
    Ok(JoinResponse),
    Err { message: String },
}

/// Connect to a bootstrap node and get cluster certs. Returns (ca, cert, key) PEMs.
async fn do_join(
    join_addr: &str,
    secret: &str,
    node_id: NodeId,
    my_addr: &str,
) -> Result<(String, String, String)> {
    let mut stream = TcpStream::connect(join_addr)
        .await
        .with_context(|| format!("cannot connect to {join_addr}"))?;

    let req = JoinRequest { secret: secret.to_owned(), node_id, addr: my_addr.to_owned() };
    let body = serde_json::to_vec(&req)?;

    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    let rlen = u32::from_be_bytes(hdr) as usize;
    let mut buf = vec![0u8; rlen];
    stream.read_exact(&mut buf).await?;

    match serde_json::from_slice::<JoinEnvelope>(&buf)? {
        JoinEnvelope::Ok(resp) => Ok((resp.ca_cert_pem, resp.node_cert_pem, resp.node_key_pem)),
        JoinEnvelope::Err { message } => anyhow::bail!("join rejected: {message}"),
    }
}

// Peer RPC listener

/// Dispatches one mTLS peer connection to the Raft node.
async fn handle_peer(stream: tokio_rustls::server::TlsStream<TcpStream>, raft: Arc<ClusterRaft>) {
    let (mut read_half, write_half) = tokio::io::split(stream);
    let mut write_half = write_half;

    let mut hdr = [0u8; 4];
    if read_half.read_exact(&mut hdr).await.is_err() {
        return;
    }
    let rlen = u32::from_be_bytes(hdr) as usize;
    let mut buf = vec![0u8; rlen];
    if read_half.read_exact(&mut buf).await.is_err() {
        return;
    }

    let resp_bytes = match serde_json::from_slice::<RpcRequest>(&buf) {
        Ok(RpcRequest::AppendEntries(req)) => match raft.append_entries(req).await {
            Ok(r) => serde_json::to_vec(&RpcResponse::<_> { ok: Some(r), err: None }).unwrap(),
            Err(e) => serde_json::to_vec(&RpcResponse::<()> { ok: None, err: Some(e.to_string()) }).unwrap(),
        },
        Ok(RpcRequest::Vote(req)) => match raft.vote(req).await {
            Ok(r) => serde_json::to_vec(&RpcResponse::<_> { ok: Some(r), err: None }).unwrap(),
            Err(e) => serde_json::to_vec(&RpcResponse::<()> { ok: None, err: Some(e.to_string()) }).unwrap(),
        },
        Ok(RpcRequest::InstallSnapshot(req)) => match raft.install_snapshot(req).await {
            Ok(r) => serde_json::to_vec(&RpcResponse::<_> { ok: Some(r), err: None }).unwrap(),
            Err(e) => serde_json::to_vec(&RpcResponse::<()> { ok: None, err: Some(e.to_string()) }).unwrap(),
        },
        Err(e) => {
            serde_json::to_vec(&RpcResponse::<()> { ok: None, err: Some(e.to_string()) }).unwrap()
        }
    };

    let len = resp_bytes.len() as u32;
    let _ = write_half.write_all(&len.to_be_bytes()).await;
    let _ = write_half.write_all(&resp_bytes).await;
    let _ = write_half.flush().await;
}

/// Handles one plain-TCP join request from a new node.
async fn handle_join(
    mut stream: TcpStream,
    expected_secret: &str,
    ca: &Ca,
    raft: Arc<ClusterRaft>,
) {
    let result = async {
        let mut hdr = [0u8; 4];
        stream.read_exact(&mut hdr).await?;
        let rlen = u32::from_be_bytes(hdr) as usize;
        let mut buf = vec![0u8; rlen];
        stream.read_exact(&mut buf).await?;

        let req: JoinRequest = serde_json::from_slice(&buf)?;

        let envelope = if req.secret != expected_secret {
            JoinEnvelope::Err { message: "invalid secret".to_owned() }
        } else {
            match ca.issue_node_cert(req.node_id) {
                Ok((cert_pem, key_pem)) => {
                    // Add the joining node as a learner — non-blocking.
                    let node = BasicNode { addr: req.addr.clone() };
                    let _ = raft.add_learner(req.node_id, node, false).await;
                    info!(
                        node_id = req.node_id,
                        addr = req.addr,
                        "cluster: node joined"
                    );
                    JoinEnvelope::Ok(JoinResponse {
                        ca_cert_pem: ca.cert_pem.clone(),
                        node_cert_pem: cert_pem,
                        node_key_pem: key_pem,
                    })
                }
                Err(e) => JoinEnvelope::Err { message: e.to_string() },
            }
        };

        let resp = serde_json::to_vec(&envelope)?;
        stream.write_all(&(resp.len() as u32).to_be_bytes()).await?;
        stream.write_all(&resp).await?;
        stream.flush().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        error!(error = %e, "cluster: join handler error");
    }
}

// Cluster service

pub struct ClusterService {
    opts: ClusterOpts,
    raft_slot: Arc<Mutex<Option<Arc<ClusterRaft>>>>,
    config_tx: Arc<watch::Sender<Option<String>>>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ClusterService {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        if let Err(e) = self.run(&mut shutdown).await {
            error!(error = %e, "cluster: fatal error");
        }
    }
}

impl ClusterService {
    async fn run(&self, shutdown: &mut pingora::server::ShutdownWatch) -> Result<()> {
        let opts = &self.opts;

        if !opts.bootstrap && opts.join.is_none() {
            anyhow::bail!("cluster mode requires --bootstrap or --join");
        }

        let (ca_cert_pem, node_cert_pem, node_key_pem, ca) = if opts.bootstrap {
            let ca = Ca::generate().context("CA generation failed")?;
            let (cert, key) = ca.issue_node_cert(opts.node_id)?;
            let ca_pem = ca.cert_pem.clone();
            (ca_pem, cert, key, Some(ca))
        } else {
            let join_addr = opts.join.as_ref().unwrap();
            let secret = opts.secret.as_deref().unwrap_or("");
            let (ca_pem, cert, key) =
                do_join(join_addr, secret, opts.node_id, &opts.cluster_addr)
                    .await
                    .context("cluster join failed")?;
            (ca_pem, cert, key, None)
        };

        let sm = StateMachine::default();
        sm.set_config_tx(Arc::clone(&self.config_tx));

        let log_store = LogStore::default();
        let raft_config = Arc::new(RaftConfig::default().validate().unwrap());

        let client_tls = build_client_tls(&node_cert_pem, &node_key_pem, &ca_cert_pem)?;
        let net_factory = ClusterNetworkFactory { tls: client_tls };

        let raft = Arc::new(
            Raft::new(opts.node_id, raft_config, net_factory, log_store, sm.clone())
                .await
                .map_err(|e| anyhow::anyhow!("Raft init: {e:?}"))?,
        );

        if opts.bootstrap {
            let mut members = BTreeMap::new();
            members.insert(opts.node_id, BasicNode { addr: opts.cluster_addr.clone() });
            if let Err(e) = raft.initialize(members).await {
                // AlreadyInitialized is harmless on restart
                warn!(error = %e, "cluster: initialize (may already be initialized)");
            }
            info!(node_id = opts.node_id, "cluster: bootstrapped as leader");
        }

        // Publish the raft handle so ControlServer can use it.
        *self.raft_slot.lock().await = Some(Arc::clone(&raft));

        let server_tls = build_server_tls(&node_cert_pem, &node_key_pem, &ca_cert_pem)?;
        let acceptor = TlsAcceptor::from(server_tls);
        let listener = TcpListener::bind(&opts.cluster_addr)
            .await
            .with_context(|| format!("cannot bind cluster addr {}", opts.cluster_addr))?;

        info!(addr = opts.cluster_addr, "cluster: peer listener ready");

        let secret = opts.secret.clone().unwrap_or_default();
        let ca: Option<Arc<Ca>> = ca.map(Arc::new);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let raft2 = Arc::clone(&raft);
                            let acceptor2 = acceptor.clone();
                            let secret2 = secret.clone();
                            let ca2 = ca.clone();

                            tokio::spawn(async move {
                                // Peek first byte: 0x16 = TLS ClientHello, otherwise join (JSON).
                                let mut peek = [0u8; 1];
                                match stream.peek(&mut peek).await {
                                    Ok(1) if peek[0] == 0x16 => {
                                        match acceptor2.accept(stream).await {
                                            Ok(tls) => handle_peer(tls, raft2).await,
                                            Err(e) => warn!(peer = %peer_addr, error = %e, "cluster: TLS accept error"),
                                        }
                                    }
                                    Ok(_) => {
                                        if let Some(ref ca_arc) = ca2 {
                                            handle_join(stream, &secret2, ca_arc, raft2).await;
                                        } else {
                                            warn!(peer = %peer_addr, "cluster: plain join but no CA (not bootstrap node)");
                                        }
                                    }
                                    Err(e) => warn!(peer = %peer_addr, error = %e, "cluster: peek error"),
                                }
                            });
                        }
                        Err(e) => error!(error = %e, "cluster: accept error"),
                    }
                }
            }
        }

        info!("cluster: shutting down");
        raft.shutdown().await.ok();
        Ok(())
    }
}

// Public factory

pub fn new_cluster(opts: ClusterOpts) -> (ClusterHandle, ClusterService) {
    let raft_slot = Arc::new(Mutex::new(None));
    let (config_tx, config_rx) = watch::channel(None);
    let config_tx = Arc::new(config_tx);

    let handle = ClusterHandle { raft: Arc::clone(&raft_slot), config_rx };
    let service = ClusterService { opts, raft_slot, config_tx };

    (handle, service)
}

// Cluster operations

/// Submit a config YAML to the cluster via Raft. Returns when committed.
pub async fn push_config(raft: &ClusterRaft, yaml: String) -> Result<()> {
    raft.client_write(ClientRequest::SetConfig { yaml })
        .await
        .map_err(|e| anyhow::anyhow!("config push failed: {e:?}"))?;
    Ok(())
}
