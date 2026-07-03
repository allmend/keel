use openraft::BasicNode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type NodeId = u64;

openraft::declare_raft_types!(
    pub TypeConfig:
        D = ClientRequest,
        R = ClientResponse,
        NodeId = NodeId,
        Node = BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = std::io::Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
);

/// Commands that flow through the Raft log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientRequest {
    /// Push a full serialised config to the cluster.
    SetConfig { yaml: String },
    /// Mark a backend as draining on all nodes.
    DrainBackend { pool: String, address: String },
    /// Re-activate a drained backend.
    ActivateBackend { pool: String, address: String },
    /// Replicate an ACME-issued certificate so every node (including late
    /// joiners, via snapshot) serves it without re-issuing.
    SetCert { host: String, cert_pem: String, key_pem: String },
}

/// host → (cert PEM, key PEM). Replicated ACME certificates.
pub type CertMap = BTreeMap<String, (String, String)>;

/// Response from the state machine after applying a log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientResponse {
    pub ok: bool,
    pub message: Option<String>,
}

impl ClientResponse {
    pub fn ok() -> Self {
        Self { ok: true, message: None }
    }
}

/// The replicated state maintained by the state machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterState {
    /// Last applied config YAML. Applied on every node after commit.
    pub config_yaml: Option<String>,
    /// Drain overrides: "pool/addr" → true means draining.
    pub draining: BTreeMap<String, bool>,
    /// ACME certificates replicated cluster-wide.
    #[serde(default)]
    pub certs: CertMap,
}
