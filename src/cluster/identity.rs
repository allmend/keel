//! A node's identity in the cluster, kept in the state directory: its ID
//! (generated once), its certificate and key, the cluster CA certificate, and
//! the last member list it knew. Also the address it announces to the others.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::cluster::types::NodeId;

const NODE_ID_FILE: &str = "node_id";
const NODE_CERT_FILE: &str = "node.crt";
const NODE_KEY_FILE: &str = "node.key";
const CLUSTER_CA_FILE: &str = "cluster-ca.crt";
const MEMBERS_FILE: &str = "members.yaml";

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

/// What a node needs for mTLS with its peers: its certificate and key from
/// the cluster CA, and the CA certificate to verify them.
pub struct NodeTls {
    pub ca_cert_pem: String,
    pub node_cert_pem: String,
    pub node_key_pem: String,
}

/// Write-then-rename, so a crash never leaves half a file behind.
fn write_file(dir: &Path, name: &str, content: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    f.write_all(content.as_bytes()).with_context(|| format!("cannot write {}", tmp.display()))?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(name)).with_context(|| format!("cannot write {}", dir.join(name).display()))
}

pub fn save_node_tls(state_dir: &Path, tls: &NodeTls) -> Result<()> {
    write_file(state_dir, CLUSTER_CA_FILE, &tls.ca_cert_pem, 0o644)?;
    write_file(state_dir, NODE_CERT_FILE, &tls.node_cert_pem, 0o644)?;
    write_file(state_dir, NODE_KEY_FILE, &tls.node_key_pem, 0o600)
}

/// The stored certificate material, or `None` when the node never joined.
pub fn load_node_tls(state_dir: &Path) -> Result<Option<NodeTls>> {
    let read = |name: &str| -> Result<Option<String>> {
        match std::fs::read_to_string(state_dir.join(name)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", state_dir.join(name).display())),
        }
    };
    match (read(CLUSTER_CA_FILE)?, read(NODE_CERT_FILE)?, read(NODE_KEY_FILE)?) {
        (Some(ca_cert_pem), Some(node_cert_pem), Some(node_key_pem)) => {
            Ok(Some(NodeTls { ca_cert_pem, node_cert_pem, node_key_pem }))
        }
        _ => Ok(None),
    }
}

/// One entry of `members.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Member {
    #[serde(with = "id_as_string")]
    pub node_id: NodeId,
    pub addr: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MemberList {
    pub members: Vec<Member>,
}

/// The last members this node knew, rewritten on every membership change.
/// Readable for operators; a node that has lost its Raft store rejoins
/// through the members listed here.
pub fn write_members(state_dir: &Path, list: &MemberList) -> Result<()> {
    let body = serde_yml::to_string(list)?;
    let text = format!("# Last known cluster members, rewritten by Keel on every membership change.\n{body}");
    write_file(state_dir, MEMBERS_FILE, &text, 0o644)
}

pub fn read_members(state_dir: &Path) -> Result<Option<MemberList>> {
    let path = state_dir.join(MEMBERS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_yml::from_str(&text).map(Some).with_context(|| format!("{} is not readable", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// YAML integers are signed 64-bit; node IDs use all 64 bits, so they are
/// written as strings.
mod id_as_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(id)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
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
    fn node_tls_and_members_round_trip() {
        let dir = temp_dir("files");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_node_tls(&dir).unwrap().is_none());
        assert!(read_members(&dir).unwrap().is_none());

        let tls = NodeTls { ca_cert_pem: "ca".into(), node_cert_pem: "cert".into(), node_key_pem: "key".into() };
        save_node_tls(&dir, &tls).unwrap();
        let back = load_node_tls(&dir).unwrap().unwrap();
        assert_eq!((back.ca_cert_pem, back.node_cert_pem, back.node_key_pem), ("ca".into(), "cert".into(), "key".into()));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("node.key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "node key is private");

        let list = MemberList {
            members: vec![Member { node_id: u64::MAX - 1, addr: "10.0.0.7:7654".into(), role: "voter".into() }],
        };
        write_members(&dir, &list).unwrap();
        assert_eq!(read_members(&dir).unwrap(), Some(list));
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
