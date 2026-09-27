pub mod ca;
pub mod control_worker;
pub mod fanout;
pub mod remote;
pub mod worker_socket;

use crate::backend::{BackendStatus, PoolRegistry, DRAIN_ACTIVE, DRAIN_DRAINING, DRAIN_REMOVED};
use fanout::{DrainOutcome, WorkerFanout};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info};

// Wire types live in the keel-control crate, shared with keelctl.
pub use keel_control::{ControlRequest, ControlResponse};

/// Largest request line an operator connection may send: a config push
/// carries the whole config directory.
const MAX_REQUEST_LINE: u64 = 64 * 1024 * 1024;

/// Read one `\n`-terminated line of at most `max` bytes. A peer that sends
/// more without a newline is refused instead of growing the buffer.
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(reader: &mut R, max: u64) -> anyhow::Result<String> {
    let mut line = String::new();
    let n = reader.take(max + 1).read_line(&mut line).await?;
    if n as u64 > max {
        anyhow::bail!("line longer than {max} bytes");
    }
    Ok(line)
}

// Server

/// The instance control socket. The master binds it as root and hands it
/// to the control worker, which serves it.
pub struct ControlServer {
    pub listener: std::os::unix::net::UnixListener,
    pub dispatch: Arc<Dispatch>,
}

impl ControlServer {
    pub async fn run(self, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        let listener = match self.listener.set_nonblocking(true).and_then(|()| UnixListener::from_std(self.listener)) {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "control: cannot serve the instance socket");
                return;
            }
        };
        info!("control: socket ready");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let dispatch = Arc::clone(&self.dispatch);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, dispatch, None).await {
                                    error!(error = %e, "control: connection error");
                                }
                            });
                        }
                        Err(e) => error!(error = %e, "control: accept error"),
                    }
                }
            }
        }
    }
}

// Dispatch

/// Where a control command is answered: the control worker, which asks the
/// workers for their view (connections, health, drain progress live in each
/// worker) and commits changes through the cluster.
pub struct Dispatch {
    pub fanout: WorkerFanout,
    pub cluster: crate::cluster::ClusterHandle,
}

impl Dispatch {
    pub fn new(fanout: WorkerFanout, cluster: crate::cluster::ClusterHandle) -> Arc<Self> {
        Arc::new(Dispatch { fanout, cluster })
    }

    /// Drain `address` on this node's workers at once, then commit it so every
    /// node, and every worker started later, keeps it drained. The workers'
    /// answer also checks the address: one in no pool is refused before
    /// anything is committed.
    async fn drain(&self, address: &str) -> Result<DrainOutcome, String> {
        let outcome = self.fanout.drain(address).await?;
        self.cluster
            .drain(address.to_owned())
            .await
            .map_err(|e| format!("drained on this node, but not committed to the cluster: {e:#}"))?;
        Ok(outcome)
    }
}

// Connection handler

/// Handle one control connection over any byte stream (Unix socket or mTLS
/// TCP). `audit` carries the remote client identity (`cn@addr`) when the
/// transport is remote; local socket connections pass `None`.
pub async fn handle_connection<S: AsyncRead + AsyncWrite + Send + 'static>(
    stream: S,
    dispatch: Arc<Dispatch>,
    audit: Option<String>,
) -> anyhow::Result<()> {
    let (reader_half, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader_half);
    let line = match read_line_capped(&mut reader, MAX_REQUEST_LINE).await {
        Ok(line) => line,
        Err(e) => {
            write_line(&mut writer, &ControlResponse::err(format!("invalid request: {e}"))).await?;
            return Ok(());
        }
    };

    if line.trim().is_empty() {
        return Ok(());
    }

    let request: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            write_line(&mut writer, &ControlResponse::err(format!("invalid request: {e}"))).await?;
            return Ok(());
        }
    };

    if let Some(who) = &audit {
        info!(client = who, command = request.name(), "control: remote command");
    }

    let cluster = &dispatch.cluster;

    match request {
        ControlRequest::Status => {
            write_line(&mut writer, &dispatch.fanout.status().await).await?;
        }

        ControlRequest::BackendList { pool } => {
            write_line(&mut writer, &dispatch.fanout.backend_list(&pool).await).await?;
        }

        ControlRequest::BackendDrain { address, wait } => {
            let outcome = match dispatch.drain(&address).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    write_line(&mut writer, &ControlResponse::err(e)).await?;
                    return Ok(());
                }
            };

            write_line(
                &mut writer,
                &ControlResponse::ok(serde_json::json!({
                    "pools": outcome.pools,
                    "workers_acknowledged": outcome.acknowledged,
                    "workers": outcome.workers,
                    "done": !wait,
                })),
            )
            .await?;

            if wait {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    // `None` is "no worker answered". Never finish on that:
                    // the backend may still be carrying connections, and
                    // reporting completion would invite the operator to take
                    // it away.
                    let conns = dispatch.fanout.connections_for(&address).await;
                    let done = conns == Some(0);
                    write_line(
                        &mut writer,
                        &ControlResponse::ok(serde_json::json!({
                            "connections": conns,
                            "done": done,
                        })),
                    )
                    .await?;
                    if done {
                        break;
                    }
                }
            }
        }

        ControlRequest::ConfigReload => {
            // Reloading reconciles this node's config directory into the
            // cluster: it becomes the next version, which every node applies.
            let resp = match crate::config::read_config_dir(&cluster.config_dir) {
                Ok(files) => cmd_config_push(cluster, files).await,
                Err(e) => ControlResponse::err(format!("{e:#}")),
            };
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ClusterStatus => {
            let resp = cmd_cluster_status(cluster).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ClusterDemote => {
            let resp = cmd_cluster_demote(cluster).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ClusterStepdown { force } => {
            let resp = cmd_cluster_stepdown(cluster, force).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ConfigPush { files } => {
            let resp = cmd_config_push(cluster, files).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::CredentialsRevokeAll => {
            let resp = match cluster.revoke_operator_credentials().await {
                Ok(()) => ControlResponse::ok(serde_json::json!({
                    "message": "control CA replaced; every earlier keelconfig is revoked — issue new ones with `keel credentials create`",
                })),
                Err(e) => ControlResponse::err(format!("{e:#}")),
            };
            write_line(&mut writer, &resp).await?;
        }
    }

    Ok(())
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> anyhow::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

// Command implementations

pub(crate) fn cmd_status(pools: &PoolRegistry, started_at: Instant) -> String {
    let uptime_secs = started_at.elapsed().as_secs();
    let all = pools.all_backends();

    let mut by_pool: BTreeMap<String, Vec<&BackendStatus>> = BTreeMap::new();
    for b in &all {
        by_pool.entry(b.pool.clone()).or_default().push(b);
    }

    let pool_data: Vec<serde_json::Value> = by_pool
        .iter()
        .map(|(name, backends)| {
            serde_json::json!({
                "name": name,
                "backends": backends.iter().map(|b| backend_json(b)).collect::<Vec<_>>(),
            })
        })
        .collect();

    ControlResponse::ok(serde_json::json!({
        "uptime_secs": uptime_secs,
        "pools": pool_data,
    }))
}

pub(crate) fn cmd_backend_list(pools: &PoolRegistry, pool_name: &str) -> String {
    if !pools.has_pool(pool_name) {
        return ControlResponse::err(format!("pool '{pool_name}' not found"));
    }
    let backends = pools.backends_for_pool(pool_name);
    ControlResponse::ok(serde_json::json!({
        "pool": pool_name,
        "backends": backends.iter().map(backend_json).collect::<Vec<_>>(),
    }))
}

fn backend_json(b: &BackendStatus) -> serde_json::Value {
    let state = match b.drain_state {
        DRAIN_ACTIVE => "active",
        DRAIN_DRAINING => "draining",
        DRAIN_REMOVED => "removed",
        _ => "unknown",
    };
    let health = match (b.ejected, b.healthy) {
        (true, _) => "ejected",
        (false, None) => "unchecked",
        (false, Some(true)) => "healthy",
        (false, Some(false)) => "unhealthy",
    };
    let reason = if b.ejected { &b.eject_reason } else { &b.health_reason };
    serde_json::json!({
        "address": b.address,
        "state": state,
        "connections": b.connections,
        "health": health,
        "health_reason": reason,
    })
}

async fn cmd_cluster_status(ch: &crate::cluster::ClusterHandle) -> String {
    let Some(raft) = ch.raft().await else {
        return ControlResponse::err("cluster not yet initialized");
    };
    let m = raft.metrics().borrow().clone();
    let role = if m.current_leader == Some(m.id) { "leader" } else { "follower" };
    let voters: std::collections::BTreeSet<_> =
        m.membership_config.membership().voter_ids().collect();
    let members: Vec<serde_json::Value> = m
        .membership_config
        .membership()
        .nodes()
        .map(|(id, node)| {
            serde_json::json!({
                "id": id,
                "addr": node.addr,
                "role": if voters.contains(id) { "voter" } else { "learner" },
            })
        })
        .collect();
    ControlResponse::ok(serde_json::json!({
        "role": role,
        "node_id": m.id,
        "term": m.current_term,
        "leader_id": m.current_leader,
        "last_committed": m.last_applied.map(|l| l.index),
        "membership": members,
    }))
}

async fn cmd_cluster_demote(ch: &crate::cluster::ClusterHandle) -> String {
    let Some(raft) = ch.raft().await else {
        return ControlResponse::err("cluster not yet initialized");
    };
    let m = raft.metrics().borrow().clone();
    if m.current_leader != Some(m.id) {
        return ControlResponse::err("this node is not the leader");
    }
    match raft.trigger().elect().await {
        Ok(()) => ControlResponse::ok(serde_json::json!({
            "message": "leadership transfer requested; a new election will begin"
        })),
        Err(e) => ControlResponse::err(e.to_string()),
    }
}

async fn cmd_cluster_stepdown(ch: &crate::cluster::ClusterHandle, force: bool) -> String {
    let Some(raft) = ch.raft().await else {
        return ControlResponse::err("cluster not yet initialized");
    };
    let Some(tls) = ch.client_tls().await else {
        return ControlResponse::err("cluster not yet initialized");
    };
    match crate::cluster::stepdown(&raft, &tls, force).await {
        Ok(message) => ControlResponse::ok(serde_json::json!({ "message": message })),
        Err(e) => ControlResponse::err(format!("{e:#}")),
    }
}

async fn cmd_config_push(ch: &crate::cluster::ClusterHandle, files: crate::config::FileSet) -> String {
    match ch.push_config(files).await {
        Ok(version) => ControlResponse::ok(serde_json::json!({
            "message": format!("config version {version} committed; every node applies it"),
            "version": version,
        })),
        Err(e) => ControlResponse::err(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::read_line_capped;

    #[tokio::test]
    async fn a_line_longer_than_the_cap_is_refused() {
        let mut short = tokio::io::BufReader::new(&b"{\"a\":1}\nrest"[..]);
        assert_eq!(read_line_capped(&mut short, 16).await.unwrap(), "{\"a\":1}\n");
        let mut long = tokio::io::BufReader::new(&[b'x'; 64][..]);
        assert!(read_line_capped(&mut long, 16).await.is_err());
    }
}
