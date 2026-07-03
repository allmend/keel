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

// Wire-frame limits — every length-prefixed read is capped before allocating so a
// peer-supplied length cannot drive a multi-gigabyte allocation (DoS).
/// Join requests/responses are small JSON (secret + node id, or a few PEMs).
pub(crate) const MAX_JOIN_FRAME: usize = 64 * 1024; // 64 KiB
/// Raft AppendEntries / InstallSnapshot can be larger; still bounded.
pub(crate) const MAX_RPC_FRAME: usize = 64 * 1024 * 1024; // 64 MiB

/// Read a 4-byte big-endian length prefix, reject lengths above `max`, then read
/// exactly that many bytes. Caps the allocation a remote peer can force.
pub(crate) async fn read_frame<R>(reader: &mut R, max: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncReadExt + Unpin,
{
    let mut hdr = [0u8; 4];
    reader.read_exact(&mut hdr).await?;
    let rlen = u32::from_be_bytes(hdr) as usize;
    if rlen > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {rlen} exceeds maximum {max}"),
        ));
    }
    let mut buf = vec![0u8; rlen];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

// Join-channel encryption
//
// The join exchange happens over plain TCP before any mTLS identity exists, so it
// must protect both the request and the response: the response carries the new
// node's private key and the cluster CA. We derive a symmetric AEAD key from the
// shared secret and encrypt both directions. A passive eavesdropper on the segment
// learns nothing; an active attacker without the secret cannot decrypt the request
// or forge a response. Successful AEAD decryption is itself proof that the peer
// holds the secret — so the secret is never transmitted, not even encrypted.
//
// Residual risk: if the shared secret is low-entropy, a captured exchange is open
// to offline brute force. Operators must use a high-entropy token (e.g. the output
// of `openssl rand -hex 32`). This is documented in the cluster setup docs.

const JOIN_KEY_CONTEXT: &[u8] = b"keel-cluster-join-v1\0";

/// Derive the AEAD key for the join channel from the shared secret.
fn join_key(secret: &str) -> ring::aead::LessSafeKey {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(JOIN_KEY_CONTEXT);
    ctx.update(secret.as_bytes());
    let digest = ctx.finish();
    // SHA-256 output (32 bytes) is exactly a ChaCha20-Poly1305 key.
    let unbound = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, digest.as_ref())
        .expect("32-byte key is valid for CHACHA20_POLY1305");
    ring::aead::LessSafeKey::new(unbound)
}

/// Seal `plaintext` as `nonce(12) || ciphertext || tag` using a fresh random nonce.
fn join_seal(key: &ring::aead::LessSafeKey, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    use ring::rand::SecureRandom;
    let mut nonce_bytes = [0u8; ring::aead::NONCE_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("rng failure"))?;
    let nonce = ring::aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, ring::aead::Aad::empty(), &mut in_out)
        .map_err(|_| anyhow::anyhow!("seal failure"))?;
    let mut out = Vec::with_capacity(ring::aead::NONCE_LEN + in_out.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Ok(out)
}

/// Open a `nonce(12) || ciphertext || tag` frame. Returns None on any auth failure
/// (wrong secret or tampering) without distinguishing the cause.
fn join_open(key: &ring::aead::LessSafeKey, framed: &[u8]) -> Option<Vec<u8>> {
    if framed.len() < ring::aead::NONCE_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = framed.split_at(ring::aead::NONCE_LEN);
    let nonce = ring::aead::Nonce::try_assume_unique_for_key(nonce_bytes).ok()?;
    let mut in_out = ciphertext.to_vec();
    let plaintext = key.open_in_place(nonce, ring::aead::Aad::empty(), &mut in_out).ok()?;
    Some(plaintext.to_vec())
}

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
    /// mTLS client config for peer RPCs (set together with `raft`). Used by the
    /// control plane to forward commands (e.g. stepdown) to the leader.
    pub client_tls: Arc<Mutex<Option<Arc<rustls::ClientConfig>>>>,
}

impl ClusterHandle {
    pub async fn raft(&self) -> Option<Arc<ClusterRaft>> {
        self.raft.lock().await.clone()
    }

    pub async fn client_tls(&self) -> Option<Arc<rustls::ClientConfig>> {
        self.client_tls.lock().await.clone()
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

/// Why a join attempt failed — decides whether retrying can ever help.
enum JoinError {
    /// Transient network trouble (listener not up yet, connection reset, …).
    Retryable(anyhow::Error),
    /// Retrying cannot succeed: wrong secret, protocol mismatch, or an explicit
    /// rejection from the cluster.
    Fatal(anyhow::Error),
}

/// Connect to a bootstrap node and get cluster certs. Returns (ca, cert, key) PEMs.
async fn do_join(
    join_addr: &str,
    secret: &str,
    node_id: NodeId,
    my_addr: &str,
) -> std::result::Result<(String, String, String), JoinError> {
    let mut stream = TcpStream::connect(join_addr)
        .await
        .map_err(|e| JoinError::Retryable(anyhow::anyhow!("cannot connect to {join_addr}: {e}")))?;

    let key = join_key(secret);
    let req = JoinRequest { node_id, addr: my_addr.to_owned() };
    let body = serde_json::to_vec(&req)
        .map_err(|e| JoinError::Fatal(e.into()))
        .and_then(|b| join_seal(&key, &b).map_err(JoinError::Fatal))?;

    let send = async {
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
        read_frame(&mut stream, MAX_JOIN_FRAME).await
    };
    let buf = send.await.map_err(|e| JoinError::Retryable(e.into()))?;

    // A frame that fails to decrypt means the secrets differ — retrying with the
    // same secret can never succeed.
    let plaintext = join_open(&key, &buf).ok_or_else(|| {
        JoinError::Fatal(anyhow::anyhow!(
            "join response decryption failed — wrong cluster secret?"
        ))
    })?;

    match serde_json::from_slice::<JoinEnvelope>(&plaintext).map_err(|e| JoinError::Fatal(e.into()))? {
        JoinEnvelope::Ok(resp) => Ok((resp.ca_cert_pem, resp.node_cert_pem, resp.node_key_pem)),
        JoinEnvelope::Err { message } => {
            Err(JoinError::Fatal(anyhow::anyhow!("join rejected: {message}")))
        }
    }
}

/// Initial delay between join attempts; doubles per failure up to the max.
const JOIN_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(1);
const JOIN_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Retry `do_join` with exponential backoff until it succeeds, fails fatally, or
/// shutdown is requested (returns `Ok(None)` in that case). Nodes are commonly
/// started simultaneously by a supervisor, so "the bootstrap listener is not up
/// yet" is the normal case, not an error worth dying over.
async fn join_with_retry(
    join_addr: &str,
    secret: &str,
    node_id: NodeId,
    my_addr: &str,
    shutdown: &mut pingora::server::ShutdownWatch,
) -> Result<Option<(String, String, String)>> {
    let mut delay = JOIN_RETRY_INITIAL;
    let mut attempt = 1u32;
    loop {
        match do_join(join_addr, secret, node_id, my_addr).await {
            Ok(certs) => return Ok(Some(certs)),
            Err(JoinError::Fatal(e)) => {
                return Err(e.context("cluster join failed (not retryable)"));
            }
            Err(JoinError::Retryable(e)) => {
                warn!(
                    attempt,
                    retry_in_secs = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "cluster: join attempt failed; retrying"
                );
                tokio::select! {
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return Ok(None);
                        }
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(JOIN_RETRY_MAX);
                attempt += 1;
            }
        }
    }
}

// Peer RPC listener

/// Dispatches one mTLS peer connection to the Raft node.
async fn handle_peer(stream: tokio_rustls::server::TlsStream<TcpStream>, raft: Arc<ClusterRaft>) {
    let (mut read_half, write_half) = tokio::io::split(stream);
    let mut write_half = write_half;

    let buf = match read_frame(&mut read_half, MAX_RPC_FRAME).await {
        Ok(b) => b,
        Err(_) => return,
    };

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
        Ok(RpcRequest::StepDown { node_id }) => match apply_stepdown(&raft, node_id).await {
            Ok(message) => serde_json::to_vec(&RpcResponse::<_> {
                ok: Some(crate::cluster::network::StepDownReply { message }),
                err: None,
            })
            .unwrap(),
            Err(e) => serde_json::to_vec(&RpcResponse::<()> { ok: None, err: Some(format!("{e:#}")) }).unwrap(),
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
        let key = join_key(expected_secret);
        let buf = read_frame(&mut stream, MAX_JOIN_FRAME).await?;

        // Successful AEAD decryption proves the peer holds the shared secret. On
        // failure, still send a sealed rejection: a genuine joiner with the wrong
        // secret cannot decrypt it and fails fast ("wrong cluster secret?") instead
        // of retrying an early-EOF forever. An attacker learns nothing beyond the
        // rejection itself, which the dropped connection already revealed.
        let Some(plaintext) = join_open(&key, &buf) else {
            warn!("cluster: join rejected — could not decrypt request (invalid secret)");
            let envelope = JoinEnvelope::Err { message: "invalid secret".to_owned() };
            let resp = join_seal(&key, &serde_json::to_vec(&envelope)?)?;
            stream.write_all(&(resp.len() as u32).to_be_bytes()).await?;
            stream.write_all(&resp).await?;
            stream.flush().await?;
            return Ok(());
        };

        let req: JoinRequest = serde_json::from_slice(&plaintext)?;

        let envelope = match ca.issue_node_cert(req.node_id) {
            Ok((cert_pem, key_pem)) => {
                // Add the joining node as a learner — non-blocking.
                let node = BasicNode { addr: req.addr.clone() };
                let _ = raft.add_learner(req.node_id, node, false).await;
                info!(node_id = req.node_id, addr = req.addr, "cluster: node joined");
                JoinEnvelope::Ok(JoinResponse {
                    ca_cert_pem: ca.cert_pem.clone(),
                    node_cert_pem: cert_pem,
                    node_key_pem: key_pem,
                })
            }
            Err(e) => JoinEnvelope::Err { message: e.to_string() },
        };

        let joined = matches!(envelope, JoinEnvelope::Ok(_));
        let resp = join_seal(&key, &serde_json::to_vec(&envelope)?)?;
        stream.write_all(&(resp.len() as u32).to_be_bytes()).await?;
        stream.write_all(&resp).await?;
        stream.flush().await?;

        // Promote the joiner to voter once it has its certs and its log has
        // caught up. Without promotion the bootstrap node stays the only voter
        // forever and the documented quorum model (3 nodes = 2 of 3) never holds.
        if joined {
            promote_to_voter(raft, req.node_id, req.addr).await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        error!(error = %e, "cluster: join handler error");
    }
}

/// How long the leader waits for a new learner's log to catch up before giving
/// up on promoting it (it stays a learner and holds no quorum weight).
const PROMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Wait for a newly joined learner to catch up, then promote it to voter so it
/// counts toward quorum. Failures are logged, never fatal — the node keeps
/// working as a learner.
async fn promote_to_voter(raft: Arc<ClusterRaft>, node_id: NodeId, addr: String) {
    let m = raft.metrics().borrow().clone();
    if m.membership_config.membership().voter_ids().any(|v| v == node_id) {
        return;
    }

    let node = BasicNode { addr };
    // Blocking add_learner returns once the leader sees the learner's log caught
    // up — replication retries internally while the joiner finishes starting.
    match tokio::time::timeout(PROMOTE_TIMEOUT, raft.add_learner(node_id, node, true)).await {
        Err(_) => {
            warn!(
                node_id,
                "cluster: learner did not catch up within {}s; leaving as learner",
                PROMOTE_TIMEOUT.as_secs()
            );
            return;
        }
        Ok(Err(e)) => {
            warn!(node_id, error = %e, "cluster: add_learner failed; leaving as learner");
            return;
        }
        Ok(Ok(_)) => {}
    }

    let mut ids = std::collections::BTreeSet::new();
    ids.insert(node_id);
    match raft.change_membership(openraft::ChangeMembers::AddVoterIds(ids), false).await {
        Ok(_) => info!(node_id, "cluster: node promoted to voter"),
        Err(e) => warn!(node_id, error = %e, "cluster: voter promotion failed; node remains a learner"),
    }
}

// Cluster service

pub struct ClusterService {
    opts: ClusterOpts,
    raft_slot: Arc<Mutex<Option<Arc<ClusterRaft>>>>,
    tls_slot: Arc<Mutex<Option<Arc<rustls::ClientConfig>>>>,
    config_tx: Arc<watch::Sender<Option<String>>>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ClusterService {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        if let Err(e) = self.run(&mut shutdown).await {
            // Exit rather than keep serving as a zombie: the operator asked for
            // cluster mode, and a node whose control plane is dead looks healthy
            // to a supervisor while silently never receiving config changes.
            error!(error = %format!("{e:#}"), "cluster: fatal error — exiting");
            std::process::exit(1);
        }
    }
}

impl ClusterService {
    async fn run(&self, shutdown: &mut pingora::server::ShutdownWatch) -> Result<()> {
        let opts = &self.opts;

        if !opts.bootstrap && opts.join.is_none() {
            anyhow::bail!("cluster mode requires --bootstrap or --join");
        }

        // A non-empty shared secret is mandatory. Without it the join listener would
        // hand a CA-signed mTLS identity to any peer that can reach the cluster port,
        // which is a full cluster takeover. Refuse to start rather than run open.
        let secret = opts.secret.as_deref().unwrap_or("").to_owned();
        if secret.is_empty() {
            anyhow::bail!(
                "cluster mode requires a non-empty shared secret \
                 (--secret or cluster.secret in config); refusing to start with an open join listener"
            );
        }

        let (ca_cert_pem, node_cert_pem, node_key_pem, ca) = if opts.bootstrap {
            let ca = Ca::generate().context("CA generation failed")?;
            let (cert, key) = ca.issue_node_cert(opts.node_id)?;
            let ca_pem = ca.cert_pem.clone();
            (ca_pem, cert, key, Some(ca))
        } else {
            let join_addr = opts.join.as_ref().unwrap();
            let Some((ca_pem, cert, key)) =
                join_with_retry(join_addr, &secret, opts.node_id, &opts.cluster_addr, shutdown)
                    .await?
            else {
                // Shutdown requested while still trying to join.
                return Ok(());
            };
            (ca_pem, cert, key, None)
        };

        let sm = StateMachine::default();
        sm.set_config_tx(Arc::clone(&self.config_tx));

        let log_store = LogStore::default();
        let raft_config = Arc::new(RaftConfig::default().validate().unwrap());

        let client_tls = build_client_tls(&node_cert_pem, &node_key_pem, &ca_cert_pem)?;
        *self.tls_slot.lock().await = Some(Arc::clone(&client_tls));
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

#[cfg(test)]
mod tests {
    use super::{join_key, join_open, join_seal};

    #[test]
    fn join_roundtrip_same_secret() {
        let key = join_key("a-high-entropy-cluster-token");
        let sealed = join_seal(&key, b"hello cluster").unwrap();
        // Ciphertext must not contain the plaintext.
        assert!(!sealed.windows(5).any(|w| w == b"hello"));
        assert_eq!(join_open(&key, &sealed).unwrap(), b"hello cluster");
    }

    #[test]
    fn join_wrong_secret_rejected() {
        let sealed = join_seal(&join_key("correct-secret"), b"payload").unwrap();
        assert!(join_open(&join_key("wrong-secret"), &sealed).is_none());
    }

    #[test]
    fn join_tamper_rejected() {
        let key = join_key("secret");
        let mut sealed = join_seal(&key, b"payload").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff; // flip a tag bit
        assert!(join_open(&key, &sealed).is_none());
    }

    #[test]
    fn join_nonces_differ_per_message() {
        let key = join_key("secret");
        let a = join_seal(&key, b"x").unwrap();
        let b = join_seal(&key, b"x").unwrap();
        // Random nonce ⇒ identical plaintext produces different frames.
        assert_ne!(a, b);
    }
}

// Public factory

pub fn new_cluster(opts: ClusterOpts) -> (ClusterHandle, ClusterService) {
    let raft_slot = Arc::new(Mutex::new(None));
    let tls_slot = Arc::new(Mutex::new(None));
    let (config_tx, config_rx) = watch::channel(None);
    let config_tx = Arc::new(config_tx);

    let handle = ClusterHandle {
        raft: Arc::clone(&raft_slot),
        config_rx,
        client_tls: Arc::clone(&tls_slot),
    };
    let service = ClusterService { opts, raft_slot, tls_slot, config_tx };

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

// Stepdown
//
// A stepdown gracefully removes the local node from the cluster: the removal is
// committed to the Raft log (so every remaining node accepts it), and if the
// leaving node is the leader, openraft steps it down once the change commits and
// the remaining voters elect a new leader.

/// How long to wait for the membership change to commit before declaring that
/// the cluster (probably) has no quorum for it.
const STEPDOWN_COMMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Per-peer TCP reachability probe timeout for the pre-stepdown quorum check.
const STEPDOWN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Gracefully remove the local node from the cluster. Runs on the leaving node.
///
/// Before committing anything, the remaining voters are probed over TCP: if fewer
/// than a majority of the post-stepdown cluster are reachable, the cluster would
/// lose quorum and the stepdown is refused unless `force` is set.
pub async fn stepdown(
    raft: &ClusterRaft,
    client_tls: &Arc<rustls::ClientConfig>,
    force: bool,
) -> Result<String> {
    let m = raft.metrics().borrow().clone();
    let my_id = m.id;
    let membership = m.membership_config.membership().clone();
    let voters: std::collections::BTreeSet<NodeId> = membership.voter_ids().collect();
    let addrs: BTreeMap<NodeId, String> =
        membership.nodes().map(|(id, n)| (*id, n.addr.clone())).collect();

    let Some(leader_id) = m.current_leader else {
        anyhow::bail!(
            "cluster has no leader — a membership change cannot be committed right now; \
             retry once a leader is elected"
        );
    };

    if voters.contains(&my_id) {
        let remaining: Vec<(NodeId, String)> = voters
            .iter()
            .filter(|id| **id != my_id)
            .filter_map(|id| addrs.get(id).map(|a| (*id, a.clone())))
            .collect();

        if remaining.is_empty() {
            anyhow::bail!(
                "this node is the only voter in the cluster; stepping down would destroy it. \
                 Shut the node down instead (the membership change could never commit)"
            );
        }

        // Quorum-loss check: after stepdown the remaining voters must be able to
        // form a majority among themselves. Probe their cluster listeners.
        let reachable = probe_reachable(&remaining).await;
        let needed = remaining.len() / 2 + 1;
        if reachable < needed {
            let msg = format!(
                "Performing this action would cause the cluster to lose quorum: \
                 after stepdown {needed} of {} remaining voter(s) must be reachable \
                 to commit changes, but only {reachable} responded.",
                remaining.len(),
            );
            if !force {
                anyhow::bail!("{msg} Refusing to step down — re-run with --force to attempt anyway.");
            }
            warn!("cluster: {msg} Proceeding due to --force");
        }
    }

    if leader_id == my_id {
        let msg = apply_stepdown(raft, my_id).await?;
        Ok(format!("{msg}. Leadership handed over to the remaining voters; it is safe to stop this node"))
    } else {
        let leader_addr = addrs
            .get(&leader_id)
            .with_context(|| format!("address of leader {leader_id} unknown"))?;
        let msg = network::send_stepdown(leader_addr, Arc::clone(client_tls), my_id).await?;
        Ok(format!("{msg}. It is safe to stop this node"))
    }
}

/// Commit the membership change that removes `node_id`. Must run on the leader —
/// either locally (leader stepping itself down; openraft demotes it after commit)
/// or on behalf of a follower that forwarded a StepDown RPC.
///
/// `change_membership` returns only after the change is committed by quorum, so a
/// successful return means every remaining node has accepted the stepdown.
pub async fn apply_stepdown(raft: &ClusterRaft, node_id: NodeId) -> Result<String> {
    let m = raft.metrics().borrow().clone();
    if m.current_leader != Some(m.id) {
        anyhow::bail!(
            "this node is not the leader (current leader: {}); cannot apply stepdown",
            m.current_leader.map(|l| l.to_string()).unwrap_or_else(|| "none".to_owned())
        );
    }

    let membership = m.membership_config.membership().clone();
    let voters: std::collections::BTreeSet<NodeId> = membership.voter_ids().collect();
    if !membership.nodes().any(|(id, _)| *id == node_id) {
        anyhow::bail!("node {node_id} is not a cluster member");
    }

    let mut remove = std::collections::BTreeSet::new();
    remove.insert(node_id);
    let change = if voters.contains(&node_id) {
        if voters.len() == 1 {
            anyhow::bail!("node {node_id} is the only voter; the cluster cannot remove it");
        }
        openraft::ChangeMembers::RemoveVoters(remove)
    } else {
        // Learners hold no quorum weight; just drop the node entry.
        openraft::ChangeMembers::RemoveNodes(remove)
    };

    // retain=false removes the node from the membership entirely (not learner).
    match tokio::time::timeout(STEPDOWN_COMMIT_TIMEOUT, raft.change_membership(change, false)).await
    {
        Err(_) => anyhow::bail!(
            "membership change did not commit within {}s — the cluster has likely lost quorum",
            STEPDOWN_COMMIT_TIMEOUT.as_secs()
        ),
        Ok(Err(e)) => anyhow::bail!("membership change failed: {e}"),
        Ok(Ok(_)) => {
            info!(node_id, "cluster: node stepped down (membership change committed)");
            Ok(format!("node {node_id} removed from cluster membership (committed by quorum)"))
        }
    }
}

/// Count how many peers accept a TCP connection on their cluster address within
/// the probe timeout. A coarse liveness signal, measured from the leaving node.
async fn probe_reachable(peers: &[(NodeId, String)]) -> usize {
    let mut set = tokio::task::JoinSet::new();
    for (_, addr) in peers {
        let addr = addr.clone();
        set.spawn(async move {
            matches!(
                tokio::time::timeout(STEPDOWN_PROBE_TIMEOUT, TcpStream::connect(&addr)).await,
                Ok(Ok(_))
            )
        });
    }
    let mut up = 0;
    while let Some(res) = set.join_next().await {
        if matches!(res, Ok(true)) {
            up += 1;
        }
    }
    up
}
