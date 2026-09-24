//! A node's identity in the cluster: its ID, generated once and kept in the
//! state directory, and the address it announces to the other nodes.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::cluster::types::NodeId;

const NODE_ID_FILE: &str = "node_id";

/// Read the node ID from `<state_dir>/node_id`, or generate a random one and
/// store it there on first start. The ID stays the node's identity for as long
/// as the file exists: a file that cannot be read or parsed is an error, never
/// a reason to pick a new ID.
pub fn load_or_create_node_id(state_dir: &Path) -> Result<NodeId> {
    let path = state_dir.join(NODE_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            return text
                .trim()
                .parse()
                .with_context(|| format!("{} does not hold a node ID", path.display()));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    }

    std::fs::create_dir_all(state_dir).with_context(|| format!("cannot create {}", state_dir.display()))?;
    let id = random_id()?;
    // Write-then-rename: a crash never leaves a half-written ID behind.
    let tmp = state_dir.join(format!(".{NODE_ID_FILE}.tmp"));
    std::fs::write(&tmp, format!("{id}\n")).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(id)
}

fn random_id() -> Result<NodeId> {
    use ring::rand::{SecureRandom, SystemRandom};
    let rng = SystemRandom::new();
    loop {
        let mut bytes = [0u8; 8];
        rng.fill(&mut bytes).map_err(|_| anyhow!("no system random source"))?;
        let id = u64::from_be_bytes(bytes);
        if id != 0 {
            return Ok(id);
        }
    }
}

/// The address announced to the other nodes: `advertise`, or `addr` when it is
/// not set. An unspecified address (0.0.0.0, ::) is only valid to bind: no
/// other node can connect to it.
pub fn announce_addr(addr: &str, advertise: Option<&str>) -> Result<String> {
    let (key, value) = match advertise {
        Some(a) => ("cluster.advertise", a),
        None => ("cluster.addr", addr),
    };
    if value.parse::<SocketAddr>().is_ok_and(|sa| sa.ip().is_unspecified()) {
        bail!(
            "{key} is {value}, which other nodes cannot connect to; \
             set cluster.advertise to the address they reach this node at"
        );
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn node_id_is_generated_once_and_kept() {
        let dir = temp_dir("keep");
        let first = load_or_create_node_id(&dir).unwrap();
        assert_ne!(first, 0);
        assert_eq!(load_or_create_node_id(&dir).unwrap(), first);
        assert_eq!(std::fs::read_to_string(dir.join("node_id")).unwrap(), format!("{first}\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_state_dirs_get_different_ids() {
        let (a, b) = (temp_dir("a"), temp_dir("b"));
        assert_ne!(load_or_create_node_id(&a).unwrap(), load_or_create_node_id(&b).unwrap());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn unreadable_node_id_is_an_error_not_a_new_identity() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("node_id"), "not a number\n").unwrap();
        let err = load_or_create_node_id(&dir).unwrap_err().to_string();
        assert!(err.contains("does not hold a node ID"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("node_id")).unwrap(), "not a number\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn announce_defaults_to_the_bind_address() {
        assert_eq!(announce_addr("10.0.0.1:7654", None).unwrap(), "10.0.0.1:7654");
        assert_eq!(announce_addr("0.0.0.0:7654", Some("10.0.0.1:7654")).unwrap(), "10.0.0.1:7654");
    }

    #[test]
    fn unspecified_announce_address_is_refused() {
        let err = announce_addr("0.0.0.0:7654", None).unwrap_err().to_string();
        assert!(err.contains("cluster.addr is 0.0.0.0:7654") && err.contains("cluster.advertise"), "{err}");
        let err = announce_addr("10.0.0.1:7654", Some("[::]:7654")).unwrap_err().to_string();
        assert!(err.contains("cluster.advertise is [::]:7654"), "{err}");
    }
}
