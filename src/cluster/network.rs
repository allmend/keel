use std::io;
use std::sync::Arc;

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RaftNetwork, RaftNetworkFactory};
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::cluster::types::{NodeId, TypeConfig};

// Wire protocol: 4-byte big-endian length prefix, then JSON body.

// Externally tagged (serde default) on purpose: internally-tagged enums are
// buffered through serde's private Content type, which cannot round-trip the
// integer map keys inside Raft membership entries (BTreeMap<NodeId, BasicNode>).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcRequest {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
    /// Ask the receiving node (must be the leader) to remove `node_id` from
    /// the cluster membership. Sent by a follower that is stepping down.
    StepDown { node_id: NodeId },
    /// Ask the receiving node (must be the leader) to admit a joining node.
    /// Sent by the member that received the join; it issues the certificate.
    Join { node_id: NodeId, addr: String },
    /// Ask the receiving node (must be the leader) to commit a config version.
    /// Sent by the member an operator pushed to.
    PushConfig { files: crate::config::FileSet },
    /// Ask the receiving node (must be the leader) to replace the control CA.
    RevokeOperatorCredentials,
}

/// The leader's answer to a forwarded config push: the committed version.
#[derive(Serialize, Deserialize)]
pub struct PushReply {
    pub version: u64,
}

#[derive(Serialize, Deserialize)]
pub struct StepDownReply {
    pub message: String,
}

/// The leader's answer to a forwarded join: admitted, or refused with a reason.
#[derive(Serialize, Deserialize)]
pub struct JoinReply {
    pub refused: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RpcResponse<T> {
    pub ok: Option<T>,
    pub err: Option<String>,
}

pub struct ClusterNetworkFactory {
    pub tls: Arc<rustls::ClientConfig>,
}

pub struct ClusterNetwork {
    target_addr: String,
    tls: Arc<rustls::ClientConfig>,
}

impl RaftNetworkFactory<TypeConfig> for ClusterNetworkFactory {
    type Network = ClusterNetwork;

    async fn new_client(&mut self, _target: NodeId, node: &BasicNode) -> Self::Network {
        ClusterNetwork { target_addr: node.addr.clone(), tls: Arc::clone(&self.tls) }
    }
}

impl ClusterNetwork {
    async fn call<Resp>(&mut self, req: &RpcRequest) -> Result<Resp, NetworkError>
    where
        Resp: for<'de> Deserialize<'de>,
    {
        let body = serde_json::to_vec(req)
            .map_err(|e| NetworkError::new(&io::Error::new(io::ErrorKind::InvalidData, e)))?;

        let tcp = TcpStream::connect(&self.target_addr)
            .await
            .map_err(|e| NetworkError::new(&e))?;

        let connector = TlsConnector::from(Arc::clone(&self.tls));
        let domain: ServerName<'static> =
            ServerName::try_from("keel-cluster").expect("static valid server name");
        let mut stream =
            connector.connect(domain, tcp).await.map_err(|e| NetworkError::new(&e))?;

        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .map_err(|e| NetworkError::new(&e))?;
        stream.write_all(&body).await.map_err(|e| NetworkError::new(&e))?;
        stream.flush().await.map_err(|e| NetworkError::new(&e))?;

        let buf = crate::cluster::read_frame(&mut stream, crate::cluster::MAX_RPC_FRAME)
            .await
            .map_err(|e| NetworkError::new(&e))?;

        let resp: RpcResponse<Resp> = serde_json::from_slice(&buf)
            .map_err(|e| NetworkError::new(&io::Error::new(io::ErrorKind::InvalidData, e)))?;

        if let Some(err) = resp.err {
            return Err(NetworkError::new(&io::Error::other(err)));
        }
        resp.ok.ok_or_else(|| {
            NetworkError::new(&io::Error::new(io::ErrorKind::UnexpectedEof, "empty response"))
        })
    }
}

/// One-shot stepdown RPC to the leader over the mTLS peer channel. Returns the
/// leader's confirmation message once the membership change has committed.
pub(crate) async fn send_stepdown(
    addr: &str,
    tls: Arc<rustls::ClientConfig>,
    node_id: NodeId,
) -> anyhow::Result<String> {
    let mut net = ClusterNetwork { target_addr: addr.to_owned(), tls };
    let reply: StepDownReply = net
        .call(&RpcRequest::StepDown { node_id })
        .await
        .map_err(|e| anyhow::anyhow!("stepdown request to leader at {addr} failed: {e}"))?;
    Ok(reply.message)
}

/// Forward a join to the leader over the mTLS peer channel. `Ok(Err(reason))`
/// is a refusal from the leader; `Err` means the leader could not be asked.
pub(crate) async fn send_join(
    leader_addr: &str,
    tls: Arc<rustls::ClientConfig>,
    node_id: NodeId,
    addr: String,
) -> anyhow::Result<Result<(), String>> {
    let mut net = ClusterNetwork { target_addr: leader_addr.to_owned(), tls };
    let reply: JoinReply = net
        .call(&RpcRequest::Join { node_id, addr })
        .await
        .map_err(|e| anyhow::anyhow!("join request to leader at {leader_addr} failed: {e}"))?;
    Ok(reply.refused.map_or(Ok(()), Err))
}

/// Forward `keel credentials revoke-all` to the leader.
pub(crate) async fn send_revoke(leader_addr: &str, tls: Arc<rustls::ClientConfig>) -> anyhow::Result<()> {
    let mut net = ClusterNetwork { target_addr: leader_addr.to_owned(), tls };
    let _: StepDownReply = net
        .call(&RpcRequest::RevokeOperatorCredentials)
        .await
        .map_err(|e| anyhow::anyhow!("revocation request to the leader at {leader_addr} failed: {e}"))?;
    Ok(())
}

/// Forward a config push to the leader; returns the version it committed.
pub(crate) async fn send_config(
    leader_addr: &str,
    tls: Arc<rustls::ClientConfig>,
    files: crate::config::FileSet,
) -> anyhow::Result<u64> {
    let mut net = ClusterNetwork { target_addr: leader_addr.to_owned(), tls };
    let reply: PushReply = net
        .call(&RpcRequest::PushConfig { files })
        .await
        .map_err(|e| anyhow::anyhow!("config push to the leader at {leader_addr} failed: {e}"))?;
    Ok(reply.version)
}

impl RaftNetwork<TypeConfig> for ClusterNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>>
    {
        self.call(&RpcRequest::AppendEntries(rpc)).await.map_err(RPCError::Network)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        self.call(&RpcRequest::InstallSnapshot(rpc)).await.map_err(RPCError::Network)
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _opt: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.call(&RpcRequest::Vote(rpc)).await.map_err(RPCError::Network)
    }
}
