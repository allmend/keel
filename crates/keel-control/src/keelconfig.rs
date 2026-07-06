//! The keelconfig credentials file — endpoint, control CA certificate, and a
//! client certificate + key signed by that CA, in one YAML document. Created
//! on a node with `keel credentials create`; consumed by `keelctl`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const ENV_VAR: &str = "KEEL_CONFIG";
pub const CWD_NAME: &str = "keelconfig";

#[derive(Debug, Serialize, Deserialize)]
pub struct Keelconfig {
    /// `host:port` of a node's remote control listener. Any cluster node
    /// works — writes are forwarded to the leader internally.
    pub endpoint: String,
    /// Control CA certificate (PEM). Verifies the server.
    pub ca_cert: String,
    /// Client certificate (PEM), signed by the control CA. The CN is the
    /// operator name and appears in the node's audit log.
    pub client_cert: String,
    /// Client private key (PEM).
    pub client_key: String,
}

impl Keelconfig {
    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_yml::to_string(self)?)
    }

    pub fn from_yaml(s: &str) -> Result<Self> {
        Ok(serde_yml::from_str(s)?)
    }

    /// Load from an explicit path, or resolve via [`resolve_path`].
    pub fn load(explicit: Option<&str>) -> Result<Self> {
        let path = resolve_path(explicit)?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read keelconfig at {}", path.display()))?;
        Self::from_yaml(&raw).with_context(|| format!("invalid keelconfig at {}", path.display()))
    }
}

/// Resolution order: explicit `--config` path → `KEEL_CONFIG` env var →
/// `./keelconfig` → `~/.keel/config`.
pub fn resolve_path(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var(ENV_VAR) {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let cwd = PathBuf::from(CWD_NAME);
    if cwd.exists() {
        return Ok(cwd);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_cfg = PathBuf::from(home).join(".keel").join("config");
        if home_cfg.exists() {
            return Ok(home_cfg);
        }
    }
    anyhow::bail!(
        "no keelconfig found: pass --config, set {ENV_VAR}, or place one at ./{CWD_NAME} or ~/.keel/config\n\
         (create one on a keel node with: keel credentials create <name> --endpoint <host:port>)"
    )
}
