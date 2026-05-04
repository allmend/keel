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

use crate::cluster::types::{ClientRequest, ClientResponse, ClusterState, NodeId, TypeConfig};

// LOG STORE 

#[derive(Debug, Default)]
struct LogStoreData {
    vote: Option<Vote<NodeId>>,
    committed: Option<LogId<NodeId>>,
    last_purged: Option<LogId<NodeId>>,
    log: BTreeMap<u64, Entry<TypeConfig>>,
}

#[derive(Debug, Clone, Default)]
pub struct LogStore(Arc<RwLock<LogStoreData>>);

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

// STATE MACHINE 

#[derive(Debug, Default)]
struct StateMachineData {
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, BasicNode>,
    state: ClusterState,
    config_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<Option<String>>>>,
}

#[derive(Debug, Clone, Default)]
pub struct StateMachine(Arc<RwLock<StateMachineData>>);

impl StateMachine {
    pub fn set_config_tx(&self, tx: std::sync::Arc<tokio::sync::watch::Sender<Option<String>>>) {
        self.0.write().unwrap().config_tx = Some(tx);
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
                            if let Some(tx) = &d.config_tx {
                                let _ = tx.send(Some(yaml.clone()));
                            }
                            d.state.config_yaml = Some(yaml);
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
        let data: ClusterState = serde_json::from_slice(snapshot.get_ref())
            .unwrap_or_default();
        let mut d = self.0.write().unwrap();
        d.state = data;
        d.last_applied = meta.last_log_id;
        d.last_membership = meta.last_membership.clone();
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
