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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::cluster::types::{NodeId, TypeConfig};

// Wire protocol: 4-byte big-endian length prefix, then JSON body.

#[derive(Serialize, Deserialize)]
#[serde(tag = "rpc", rename_all = "snake_case")]
pub enum RpcRequest {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
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

        let mut hdr = [0u8; 4];
        stream.read_exact(&mut hdr).await.map_err(|e| NetworkError::new(&e))?;
        let rlen = u32::from_be_bytes(hdr) as usize;
        let mut buf = vec![0u8; rlen];
        stream.read_exact(&mut buf).await.map_err(|e| NetworkError::new(&e))?;

        let resp: RpcResponse<Resp> = serde_json::from_slice(&buf)
            .map_err(|e| NetworkError::new(&io::Error::new(io::ErrorKind::InvalidData, e)))?;

        if let Some(err) = resp.err {
            return Err(NetworkError::new(&io::Error::new(io::ErrorKind::Other, err)));
        }
        resp.ok.ok_or_else(|| {
            NetworkError::new(&io::Error::new(io::ErrorKind::UnexpectedEof, "empty response"))
        })
    }
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
