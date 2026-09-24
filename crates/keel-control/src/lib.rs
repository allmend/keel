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

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// A replicated config: every file of a config directory, keyed by its path
/// relative to that directory (`/`-separated).
pub type FileSet = BTreeMap<String, String>;

/// Every file below `dir`, by relative path. Names starting with `.` (editor
/// and temporary files) are skipped; every other file must be UTF-8 text.
pub fn read_file_set(dir: &Path) -> anyhow::Result<FileSet> {
    fn walk(root: &Path, dir: &Path, files: &mut FileSet) -> anyhow::Result<()> {
        let entries = std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(root, &path, files)?;
                continue;
            }
            let rel = path.strip_prefix(root).expect("below root");
            let key = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {} as text", path.display()))?;
            files.insert(key, text);
        }
        Ok(())
    }
    let mut files = FileSet::new();
    walk(dir, dir, &mut files)?;
    Ok(files)
}

/// The file set `config push <path>` sends: a directory as it is, or a single
/// file as the root `keel.yaml` of a set that holds nothing else.
pub fn push_file_set(path: &Path) -> anyhow::Result<FileSet> {
    if path.is_dir() {
        return read_file_set(path);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(FileSet::from([("keel.yaml".to_owned(), text)]))
}

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
    /// A new config version: the whole file set, which replaces the current one.
    ConfigPush { files: FileSet },
    /// Replace the control CA, revoking every operator credential.
    CredentialsRevokeAll,
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
            ControlRequest::CredentialsRevokeAll => "credentials_revoke_all",
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
