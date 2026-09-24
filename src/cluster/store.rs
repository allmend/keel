use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::{Arc, RwLock};

use openraft::{
    BasicNode, Entry, LogId, LogState, RaftLogReader, RaftSnapshotBuilder,
    Snapshot, SnapshotMeta, StoredMembership, Vote,
    storage::{LogFlushed, RaftLogStorage, RaftStateMachine},
};

use crate::cluster::persist::{Disk, Loaded, StoredSnapshot};
use crate::cluster::types::{
    CertMap, ChallengeMap, ClientRequest, ClientResponse, ClusterCaPair, ClusterState, ConfigVersion, NodeId, TypeConfig, ControlCaPair};

fn disk_err(e: anyhow::Error) -> openraft::StorageError<NodeId> {
    openraft::StorageIOError::write(openraft::AnyError::error(format!("{e:#}"))).into()
}

// Log store

/// The log in memory, written through to disk before each change is visible.
#[derive(Default)]
struct LogStoreData {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    last_purged: Option<LogId<NodeId>>,
    log: BTreeMap<u64, Entry<TypeConfig>>,
    disk: Option<Arc<Disk>>,
}

#[derive(Clone, Default)]
pub struct LogStore(Arc<RwLock<LogStoreData>>);

impl LogStore {
    /// The log as the store held it, persisted to `disk` from here on.
    pub fn persisted(disk: Arc<Disk>, loaded: &Loaded) -> Self {
        LogStore(Arc::new(RwLock::new(LogStoreData {
            vote: loaded.vote,
            committed: loaded.committed,
            last_purged: loaded.last_purged,
            log: loaded.log.clone(),
            disk: Some(disk),
        })))
    }

    fn disk(&self) -> Option<Arc<Disk>> {
        self.0.read().unwrap().disk.clone()
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, openraft::StorageError<NodeId>> {
        let d = self.0.read().unwrap();
        Ok(d.log.range(range).map(|(_, e)| e.clone()).collect())
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(
        &mut self,
    ) -> Result<LogState<TypeConfig>, openraft::StorageError<NodeId>> {
        let d = self.0.read().unwrap();
        let last = d.log.values().last().map(|e| e.log_id);
        Ok(LogState {
            last_purged_log_id: d.last_purged,
            last_log_id: last,
        })
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        if let Some(disk) = self.disk() {
            disk.save_committed(&committed).map_err(disk_err)?;
        }
        self.0.write().unwrap().committed = committed;
        Ok(())
    }

    async fn read_committed(
        &mut self,
    ) -> Result<Option<LogId<NodeId>>, openraft::StorageError<NodeId>> {
        Ok(self.0.read().unwrap().committed)
    }

    async fn save_vote(
        &mut self,
        vote: &Vote<NodeId>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        if let Some(disk) = self.disk() {
            disk.save_vote(vote).map_err(disk_err)?;
        }
        self.0.write().unwrap().vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(
        &mut self,
    ) -> Result<Option<Vote<NodeId>>, openraft::StorageError<NodeId>> {
        Ok(self.0.read().unwrap().vote)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), openraft::StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
    {
        let entries: Vec<Entry<TypeConfig>> = entries.into_iter().collect();
        if let Some(disk) = self.disk() {
            disk.append(&entries).map_err(disk_err)?;
        }
        {
            let mut d = self.0.write().unwrap();
            for e in entries {
                d.log.insert(e.log_id.index, e);
            }
        }
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        if let Some(disk) = self.disk() {
            disk.truncate(log_id.index).map_err(disk_err)?;
        }
        let mut d = self.0.write().unwrap();
        let keys: Vec<u64> = d.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys {
            d.log.remove(&k);
        }
        Ok(())
    }

    async fn purge(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        if let Some(disk) = self.disk() {
            disk.purge(&log_id).map_err(disk_err)?;
        }
        let mut d = self.0.write().unwrap();
        let keys: Vec<u64> = d.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in keys {
            d.log.remove(&k);
        }
        d.last_purged = Some(log_id);
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

// State machine

#[derive(Debug, Default)]
struct StateMachineData {
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, BasicNode>,
    state: ClusterState,
    config_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<Option<ConfigVersion>>>>,
    certs_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<CertMap>>>,
    challenges_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<ChallengeMap>>>,
    control_ca_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<ControlCaPair>>>,
    cluster_ca_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<ClusterCaPair>>>,
    disk: Option<Arc<Disk>>,
}

impl StateMachineData {
    fn set_config(&mut self, version: u64, files: crate::config::FileSet) {
        self.state.config = Some(ConfigVersion { version, files });
        if let Some(tx) = &self.config_tx {
            let _ = tx.send(self.state.config.clone());
        }
    }

    /// Replace the state with a snapshot's and tell every watcher, as apply()
    /// would have. Used for a received snapshot and for the stored one at start.
    fn restore(&mut self, meta: &SnapshotMeta<NodeId, BasicNode>, state: ClusterState) {
        self.state = state;
        self.last_applied = meta.last_log_id;
        self.last_membership = meta.last_membership.clone();
        if let Some(tx) = &self.certs_tx {
            let _ = tx.send(self.state.certs.clone());
        }
        if let Some(tx) = &self.challenges_tx {
            let _ = tx.send(self.state.challenges.clone());
        }
        if let Some(tx) = &self.control_ca_tx {
            let _ = tx.send(self.state.control_ca.clone());
        }
        if let Some(tx) = &self.cluster_ca_tx {
            let _ = tx.send(self.state.cluster_ca.clone());
        }
        if let (Some(tx), Some(config)) = (&self.config_tx, &self.state.config) {
            let _ = tx.send(Some(config.clone()));
        }
    }
}

// The error is openraft's own storage error, returned unchanged to it.
#[allow(clippy::result_large_err)]
fn decode_state(meta: &SnapshotMeta<NodeId, BasicNode>, data: &[u8]) -> Result<ClusterState, openraft::StorageError<NodeId>> {
    // A corrupt snapshot must surface as a storage error, not silently reset
    // the cluster state (config + drain map) to default.
    serde_json::from_slice(data).map_err(|e| {
        openraft::StorageIOError::read_snapshot(Some(meta.signature()), openraft::AnyError::new(&e)).into()
    })
}

#[derive(Debug, Clone, Default)]
pub struct StateMachine(Arc<RwLock<StateMachineData>>);

impl StateMachine {
    pub fn set_config_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<Option<ConfigVersion>>>) {
        self.0.write().unwrap().config_tx = Some(tx);
    }

    pub fn set_certs_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<CertMap>>) {
        self.0.write().unwrap().certs_tx = Some(tx);
    }

    pub fn set_challenges_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<ChallengeMap>>) {
        self.0.write().unwrap().challenges_tx = Some(tx);
    }

    pub fn set_control_ca_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<ControlCaPair>>) {
        self.0.write().unwrap().control_ca_tx = Some(tx);
    }

    pub fn set_cluster_ca_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<ClusterCaPair>>) {
        self.0.write().unwrap().cluster_ca_tx = Some(tx);
    }

    /// Persist snapshots to `disk` from here on, and start from the stored
    /// one. Call after the watch senders are set, so restoring notifies them.
    pub fn persisted(&self, disk: Arc<Disk>, stored: Option<&StoredSnapshot>) -> anyhow::Result<()> {
        let mut d = self.0.write().unwrap();
        d.disk = Some(disk);
        if let Some(snap) = stored {
            let state = decode_state(&snap.meta, &snap.data).map_err(|e| anyhow::anyhow!("stored snapshot: {e}"))?;
            d.restore(&snap.meta, state);
        }
        Ok(())
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<TypeConfig>, openraft::StorageError<NodeId>> {
        let d = self.0.read().unwrap();
        let data = serde_json::to_vec(&d.state).unwrap_or_default();
        let meta = SnapshotMeta {
            last_log_id: d.last_applied,
            last_membership: d.last_membership.clone(),
            snapshot_id: format!(
                "{}-{}",
                d.last_applied.map(|l| l.leader_id.to_string()).unwrap_or_default(),
                d.last_applied.map_or(0, |l| l.index)
            ),
        };
        // Stored before openraft purges the log it covers.
        if let Some(disk) = &d.disk {
            disk.save_snapshot(&StoredSnapshot { meta: meta.clone(), data: data.clone() }).map_err(|e| {
                openraft::StorageIOError::write_snapshot(Some(meta.signature()), openraft::AnyError::error(format!("{e:#}")))
            })?;
        }
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>),
        openraft::StorageError<NodeId>,
    > {
        let d = self.0.read().unwrap();
        Ok((d.last_applied, d.last_membership.clone()))
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<ClientResponse>, openraft::StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
    {
        let mut responses = Vec::new();
        let mut d = self.0.write().unwrap();

        for entry in entries {
            d.last_applied = Some(entry.log_id);

            match entry.payload {
                openraft::EntryPayload::Blank => {
                    responses.push(ClientResponse::ok());
                }
                openraft::EntryPayload::Normal(req) => {
                    let resp = match req {
                        ClientRequest::SetConfig { yaml } => {
                            let files = crate::config::FileSet::from([(crate::config::ROOT_FILE.to_owned(), yaml)]);
                            d.set_config(entry.log_id.index, files);
                            ClientResponse::ok()
                        }
                        ClientRequest::SetConfigFiles { files } => {
                            d.set_config(entry.log_id.index, files);
                            ClientResponse::ok()
                        }
                        ClientRequest::DrainBackend { pool, address } => {
                            d.state.draining.insert(format!("{pool}/{address}"), true);
                            ClientResponse::ok()
                        }
                        ClientRequest::ActivateBackend { pool, address } => {
                            d.state.draining.remove(&format!("{pool}/{address}"));
                            ClientResponse::ok()
                        }
                        ClientRequest::SetCert { host, cert_pem, key_pem } => {
                            d.state.certs.insert(host, (cert_pem, key_pem));
                            if let Some(tx) = &d.certs_tx {
                                let _ = tx.send(d.state.certs.clone());
                            }
                            ClientResponse::ok()
                        }
                        ClientRequest::SetChallenge { token, key_auth } => {
                            d.state.challenges.insert(token, key_auth);
                            if let Some(tx) = &d.challenges_tx {
                                let _ = tx.send(d.state.challenges.clone());
                            }
                            ClientResponse::ok()
                        }
                        ClientRequest::RemoveChallenge { token } => {
                            d.state.challenges.remove(&token);
                            if let Some(tx) = &d.challenges_tx {
                                let _ = tx.send(d.state.challenges.clone());
                            }
                            ClientResponse::ok()
                        }
                        ClientRequest::SetControlCa { cert_pem, key_pem } => {
                            d.state.control_ca = Some((cert_pem, key_pem));
                            if let Some(tx) = &d.control_ca_tx {
                                let _ = tx.send(d.state.control_ca.clone());
                            }
                            ClientResponse::ok()
                        }
                        ClientRequest::SetClusterCa { cert_pem, key_pem } => {
                            d.state.cluster_ca = Some((cert_pem, key_pem));
                            if let Some(tx) = &d.cluster_ca_tx {
                                let _ = tx.send(d.state.cluster_ca.clone());
                            }
                            ClientResponse::ok()
                        }
                    };
                    responses.push(resp);
                }
                openraft::EntryPayload::Membership(m) => {
                    d.last_membership = StoredMembership::new(Some(entry.log_id), m);
                    responses.push(ClientResponse::ok());
                }
            }
        }

        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, openraft::StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), openraft::StorageError<NodeId>> {
        let state = decode_state(meta, snapshot.get_ref())?;
        let mut d = self.0.write().unwrap();
        if let Some(disk) = &d.disk {
            let stored = StoredSnapshot { meta: meta.clone(), data: snapshot.get_ref().clone() };
            disk.save_snapshot(&stored).map_err(|e| {
                openraft::StorageIOError::write_snapshot(Some(meta.signature()), openraft::AnyError::error(format!("{e:#}")))
            })?;
        }
        // A snapshot is how late joiners receive replicated state that has been
        // compacted out of the log — restore() notifies watchers like apply().
        d.restore(meta, state);
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, openraft::StorageError<NodeId>> {
        let d = self.0.read().unwrap();
        if d.last_applied.is_none() {
            return Ok(None);
        }
        let data = serde_json::to_vec(&d.state).unwrap_or_default();
        let meta = SnapshotMeta {
            last_log_id: d.last_applied,
            last_membership: d.last_membership.clone(),
            snapshot_id: format!(
                "{}-{}",
                d.last_applied.map(|l| l.leader_id.to_string()).unwrap_or_default(),
                d.last_applied.map_or(0, |l| l.index)
            ),
        };
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        }))
    }
}
