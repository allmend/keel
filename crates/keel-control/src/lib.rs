//! Control protocol for Keel: wire types, the keelconfig credentials file,
//! and a synchronous client that runs each CLI command over any byte stream.
//!
//! The same protocol serves two transports:
//!   - the local Unix socket (`keel <subcommand>` on the node itself)
//!   - the remote mTLS TCP listener (`keelctl` from an operator workstation)
//!
//! One request per connection, newline-delimited JSON. Most commands get a
//! single response line; `backend drain --wait` streams status lines until
//! the drain completes.

pub mod client;
pub mod keelconfig;

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    BackendList { pool: String },
    BackendDrain { address: String, #[serde(default)] wait: bool },
    ConfigReload,
    ClusterStatus,
    ClusterDemote,
    ClusterStepdown { #[serde(default)] force: bool },
    ConfigPush { yaml: String },
}

impl ControlRequest {
    /// Stable command name for audit logging.
    pub fn name(&self) -> &'static str {
        match self {
            ControlRequest::Status => "status",
            ControlRequest::BackendList { .. } => "backend_list",
            ControlRequest::BackendDrain { .. } => "backend_drain",
            ControlRequest::ConfigReload => "config_reload",
            ControlRequest::ClusterStatus => "cluster_status",
            ControlRequest::ClusterDemote => "cluster_demote",
            ControlRequest::ClusterStepdown { .. } => "cluster_stepdown",
            ControlRequest::ConfigPush { .. } => "config_push",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok(data: impl Serialize) -> String {
        let val = serde_json::to_value(data).unwrap_or_default();
        let r = ControlResponse { ok: true, data: Some(val), error: None };
        serde_json::to_string(&r).unwrap_or_default()
    }

    pub fn err(msg: impl Into<String>) -> String {
        let r = ControlResponse { ok: false, data: None, error: Some(msg.into()) };
        serde_json::to_string(&r).unwrap_or_default()
    }
}
