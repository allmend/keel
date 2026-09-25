//! The node's Raft state on disk: log, vote, committed index and the latest
//! snapshot, in one redb file under `<state_dir>/raft/`. Every write commits
//! durably before the in-memory view changes, and opening reads and decodes
//! everything, so a damaged store is found at startup, not at some later read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use openraft::{BasicNode, CommittedLeaderId, Entry, EntryPayload, LogId, Membership, SnapshotMeta, Vote};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::cluster::types::{NodeId, TypeConfig};

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("log");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const VOTE: &str = "vote";
const COMMITTED: &str = "committed";
const LAST_PURGED: &str = "last_purged";
const SNAPSHOT: &str = "snapshot";

const STORE_FILE: &str = "store.redb";

/// The latest snapshot: the replicated state as of `meta.last_log_id`.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredSnapshot {
    pub meta: SnapshotMeta<NodeId, BasicNode>,
    pub data: Vec<u8>,
}

/// Everything the store held when it was opened.
#[derive(Default)]
pub struct Loaded {
    pub vote: Option<Vote<NodeId>>,
    pub committed: Option<LogId<NodeId>>,
    pub last_purged: Option<LogId<NodeId>>,
    pub log: BTreeMap<u64, Entry<TypeConfig>>,
    pub snapshot: Option<StoredSnapshot>,
}

impl Loaded {
    /// Nothing was ever stored: a node that has not bootstrapped or joined.
    pub fn is_empty(&self) -> bool {
        self.vote.is_none() && self.log.is_empty() && self.last_purged.is_none() && self.snapshot.is_none()
    }

    /// The membership the node last knew: the newest membership entry in the
    /// log, else the snapshot's.
    pub fn membership(&self) -> Option<Membership<NodeId, BasicNode>> {
        let from_log = self.log.values().rev().find_map(|e| match &e.payload {
            EntryPayload::Membership(m) => Some(m.clone()),
            _ => None,
        });
        from_log.or_else(|| self.snapshot.as_ref().map(|s| s.meta.last_membership.membership().clone()))
    }

    fn last_log_id(&self) -> Option<LogId<NodeId>> {
        self.log.values().last().map(|e| e.log_id).or(self.last_purged)
    }
}

pub struct Disk {
    db: Database,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Disk")
    }
}

fn io(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

impl Disk {
    /// Open (or create) the store in `dir` and read all of it back.
    pub fn open(dir: &Path) -> Result<(Arc<Disk>, Loaded)> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(STORE_FILE);
        let db = Database::create(&path).map_err(io).with_context(|| format!("cannot open {}", path.display()))?;
        // The store holds the cluster and control CA keys.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        // Create both tables so a new store reads back as empty, not as missing.
        let txn = db.begin_write().map_err(io)?;
        txn.open_table(LOG).map_err(io)?;
        txn.open_table(META).map_err(io)?;
        txn.commit().map_err(io)?;

        let disk = Disk { db };
        let loaded = disk.load().with_context(|| format!("cannot read {}", path.display()))?;
        Ok((Arc::new(disk), loaded))
    }

    fn load(&self) -> Result<Loaded> {
        let txn = self.db.begin_read().map_err(io)?;
        let meta = txn.open_table(META).map_err(io)?;
        let get = |key: &str| -> Result<Option<Vec<u8>>> {
            Ok(meta.get(key).map_err(io)?.map(|v| v.value().to_vec()))
        };
        let decode = |key: &str| -> Result<Option<serde_json::Value>> {
            get(key)?.map(|b| serde_json::from_slice(&b).with_context(|| format!("{key} is not readable"))).transpose()
        };
        let mut loaded = Loaded {
            vote: decode(VOTE)?.map(serde_json::from_value).transpose()?,
            committed: decode(COMMITTED)?.map(serde_json::from_value).transpose()?,
            last_purged: decode(LAST_PURGED)?.map(serde_json::from_value).transpose()?,
            snapshot: decode(SNAPSHOT)?.map(serde_json::from_value).transpose()?,
            log: BTreeMap::new(),
        };
        let log = txn.open_table(LOG).map_err(io)?;
        for row in log.iter().map_err(io)? {
            let (index, bytes) = row.map_err(io)?;
            let entry: Entry<TypeConfig> = serde_json::from_slice(bytes.value())
                .with_context(|| format!("log entry {} is not readable", index.value()))?;
            loaded.log.insert(index.value(), entry);
        }
        Ok(loaded)
    }

    fn put_meta(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        let txn = self.db.begin_write().map_err(io)?;
        txn.open_table(META).map_err(io)?.insert(key, bytes.as_slice()).map_err(io)?;
        txn.commit().map_err(io)
    }

    pub fn save_vote(&self, vote: &Vote<NodeId>) -> Result<()> {
        self.put_meta(VOTE, vote)
    }

    pub fn save_committed(&self, committed: &Option<LogId<NodeId>>) -> Result<()> {
        self.put_meta(COMMITTED, committed)
    }

    pub fn save_snapshot(&self, snapshot: &StoredSnapshot) -> Result<()> {
        self.put_meta(SNAPSHOT, snapshot)
    }

    pub fn append(&self, entries: &[Entry<TypeConfig>]) -> Result<()> {
        let txn = self.db.begin_write().map_err(io)?;
        {
            let mut log = txn.open_table(LOG).map_err(io)?;
            for e in entries {
                let bytes = serde_json::to_vec(e)?;
                log.insert(e.log_id.index, bytes.as_slice()).map_err(io)?;
            }
        }
        txn.commit().map_err(io)
    }

    /// Remove every entry from `index` on (a conflicting tail).
    pub fn truncate(&self, index: u64) -> Result<()> {
        let txn = self.db.begin_write().map_err(io)?;
        txn.open_table(LOG).map_err(io)?.retain_in(index.., |_, _| false).map_err(io)?;
        txn.commit().map_err(io)
    }

    /// Remove every entry up to and including `log_id`, now covered by a snapshot.
    pub fn purge(&self, log_id: &LogId<NodeId>) -> Result<()> {
        let bytes = serde_json::to_vec(&Some(*log_id))?;
        let txn = self.db.begin_write().map_err(io)?;
        txn.open_table(LOG).map_err(io)?.retain_in(..=log_id.index, |_, _| false).map_err(io)?;
        txn.open_table(META).map_err(io)?.insert(LAST_PURGED, bytes.as_slice()).map_err(io)?;
        txn.commit().map_err(io)
    }
}

/// Move a store Keel cannot use out of the way, keeping it for inspection.
/// Returns where it went.
pub fn set_aside(dir: &Path, reason: &str) -> Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = format!("{}.{reason}-{stamp}", dir.file_name().and_then(|n| n.to_str()).unwrap_or("raft"));
    let aside = dir.with_file_name(name);
    std::fs::rename(dir, &aside).with_context(|| format!("cannot move {} to {}", dir.display(), aside.display()))?;
    Ok(aside)
}

/// Forced recovery: make this node the only member, keeping the stored state.
/// Appends a membership entry `{node_id}` in a newer term and votes for itself
/// in that term; on start the node, as the only voter, elects itself and
/// commits the entry. Only on an explicit operator command: members that are
/// still alive are cut off and keep a membership of their own.
pub fn force_single_member(disk: &Disk, loaded: &mut Loaded, node_id: NodeId, addr: &str) -> Result<()> {
    let term = loaded.vote.map_or(0, |v| v.leader_id().get_term()) + 1;
    let index = loaded.last_log_id().map_or(0, |l| l.index + 1);
    let nodes = BTreeMap::from([(node_id, BasicNode { addr: addr.to_owned() })]);
    let membership = Membership::new(vec![[node_id].into()], nodes);
    let entry = Entry {
        log_id: LogId::new(CommittedLeaderId::new(term, node_id), index),
        payload: EntryPayload::Membership(membership),
    };
    let vote = Vote::new(term, node_id);
    disk.append(std::slice::from_ref(&entry))?;
    disk.save_vote(&vote)?;
    loaded.log.insert(index, entry);
    loaded.vote = Some(vote);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn blank(term: u64, index: u64) -> Entry<TypeConfig> {
        Entry { log_id: LogId::new(CommittedLeaderId::new(term, 1), index), payload: EntryPayload::Blank }
    }

    #[test]
    fn a_new_store_is_empty_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("empty");
        let (_, loaded) = Disk::open(&dir).unwrap();
        assert!(loaded.is_empty());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&dir.join(STORE_FILE))), (0o700, 0o600), "the store holds CA keys");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn everything_written_is_read_back() {
        let dir = temp_dir("roundtrip");
        {
            let (disk, _) = Disk::open(&dir).unwrap();
            disk.save_vote(&Vote::new(3, 1)).unwrap();
            disk.append(&[blank(3, 1), blank(3, 2), blank(3, 3), blank(3, 4)]).unwrap();
            disk.save_committed(&Some(LogId::new(CommittedLeaderId::new(3, 1), 3))).unwrap();
            disk.truncate(4).unwrap();
            disk.purge(&LogId::new(CommittedLeaderId::new(3, 1), 1)).unwrap();
        }
        let (_, loaded) = Disk::open(&dir).unwrap();
        assert_eq!(loaded.vote, Some(Vote::new(3, 1)));
        assert_eq!(loaded.committed.map(|l| l.index), Some(3));
        assert_eq!(loaded.last_purged.map(|l| l.index), Some(1));
        assert_eq!(loaded.log.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
        assert!(!loaded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_store_is_an_error() {
        let dir = temp_dir("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STORE_FILE), b"this is not a redb file, not even close").unwrap();
        assert!(Disk::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_set_aside_keeps_its_content() {
        let parent = temp_dir("aside");
        let dir = parent.join("raft");
        Disk::open(&dir).unwrap();
        let aside = set_aside(&dir, "corrupt").unwrap();
        assert!(!dir.exists() && aside.join(STORE_FILE).exists(), "{}", aside.display());
        assert!(aside.file_name().unwrap().to_str().unwrap().starts_with("raft.corrupt-"));
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn forced_recovery_leaves_this_node_the_only_voter() {
        let dir = temp_dir("force");
        {
            let (disk, _) = Disk::open(&dir).unwrap();
            disk.save_vote(&Vote::new(5, 2)).unwrap();
            disk.append(&[blank(5, 1), blank(5, 2)]).unwrap();
        }
        {
            // redb holds a file lock: one open handle per store.
            let (disk, mut loaded) = Disk::open(&dir).unwrap();
            assert!(Disk::open(&dir).is_err(), "a second open is refused");
            force_single_member(&disk, &mut loaded, 7, "10.0.0.7:7654").unwrap();
        }

        let (_, reread) = Disk::open(&dir).unwrap();
        let m = reread.membership().expect("membership entry");
        assert_eq!(m.voter_ids().collect::<Vec<_>>(), vec![7]);
        let last = reread.log.values().last().unwrap().log_id;
        assert_eq!((last.index, last.leader_id.term), (3, 6));
        assert_eq!(reread.vote, Some(Vote::new(6, 7)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
