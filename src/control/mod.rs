pub mod ca;
pub mod fanout;
pub mod remote;

use crate::backend::{BackendStatus, PoolRegistry, DRAIN_ACTIVE, DRAIN_DRAINING, DRAIN_REMOVED};
use async_trait::async_trait;
use fanout::WorkerFanout;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info};

// Wire types live in the keel-control crate, shared with keelctl.
pub use keel_control::{ControlRequest, ControlResponse};

// Server

pub struct ControlServer {
    pub socket_path: String,
    pub dispatch: Arc<Dispatch>,
    /// uid/gid to give the socket. The master binds as root but the socket
    /// belongs to the worker user, so operators who could reach it through
    /// group membership before still can.
    pub owner: Option<(u32, u32)>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ControlServer {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        self.run(&mut shutdown).await
    }
}

impl ControlServer {
    /// Serve the socket until `shutdown` flips. Split out of the
    /// `BackgroundService` impl so the master — which has no Pingora server —
    /// can run the same listener on its own runtime.
    pub async fn run(&self, shutdown: &mut pingora::server::ShutdownWatch) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(parent) = std::path::Path::new(&self.socket_path).parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                error!(path = self.socket_path, error = %e, "control: failed to create socket directory");
                return;
            }
            // Restrict the directory first so the socket is never traversable by
            // other users, even during the brief window before its own mode is set.
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750)) {
                error!(path = %parent.display(), error = %e, "control: failed to set socket directory permissions");
                return;
            }
        }

        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                error!(path = self.socket_path, error = %e, "control: failed to bind socket");
                return;
            }
        };

        if let Some((uid, gid)) = self.owner {
            let path = std::path::Path::new(&self.socket_path);
            if let Err(e) = nix::unistd::chown(
                path,
                Some(nix::unistd::Uid::from_raw(uid)),
                Some(nix::unistd::Gid::from_raw(gid)),
            ) {
                error!(path = self.socket_path, error = %e, "control: failed to set socket ownership");
                let _ = std::fs::remove_file(&self.socket_path);
                return;
            }
        }

        // The control protocol can drain backends, reload config, and push config
        // to the whole cluster — anyone who can open the socket owns the proxy.
        // Restrict to owner+group (0660); refuse to serve if we cannot lock it down.
        if let Err(e) =
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o660))
        {
            error!(path = self.socket_path, error = %e, "control: failed to restrict socket permissions");
            let _ = std::fs::remove_file(&self.socket_path);
            return;
        }

        info!(path = self.socket_path, "control: socket ready");

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
                        Err(e) => {
                            error!(error = %e, "control: accept error");
                        }
                    }
                }
            }
        }

        let _ = std::fs::remove_file(&self.socket_path);
        info!("control: socket closed");
    }
}

// Dispatch

/// Where a control command is answered.
///
/// `Local` answers from this process's own `PoolRegistry` — that is the
/// cluster-mode node (one process, no workers) and each worker on its own
/// per-worker socket. `Workers` is the master: it owns the instance socket
/// and fans every command out to the workers, because the drain state,
/// connection counters and health of a forked worker live only in that
/// worker.
pub enum Dispatch {
    Local {
        pools: Arc<PoolRegistry>,
        started_at: Instant,
        cluster: Option<crate::cluster::ClusterHandle>,
    },
    Workers(WorkerFanout),
}

impl Dispatch {
    pub fn local(
        pools: Arc<PoolRegistry>,
        cluster: Option<crate::cluster::ClusterHandle>,
    ) -> Arc<Self> {
        Arc::new(Dispatch::Local { pools, started_at: Instant::now(), cluster })
    }

    /// The master's dispatch: one socket per worker, merged on the way out.
    pub fn workers(control_socket: &str, workers: usize) -> Arc<Self> {
        Arc::new(Dispatch::Workers(WorkerFanout::new(control_socket, workers)))
    }

    pub fn fanout(&self) -> Option<&WorkerFanout> {
        match self {
            Dispatch::Workers(w) => Some(w),
            Dispatch::Local { .. } => None,
        }
    }

    async fn status(&self) -> String {
        match self {
            Dispatch::Local { pools, started_at, .. } => cmd_status(pools, *started_at),
            Dispatch::Workers(w) => w.status().await,
        }
    }

    async fn backend_list(&self, pool: &str) -> String {
        match self {
            Dispatch::Local { pools, .. } => cmd_backend_list(pools, pool),
            Dispatch::Workers(w) => w.backend_list(pool).await,
        }
    }

    /// Mark a backend draining. `Ok` carries the pools it was found in.
    async fn drain(&self, address: &str) -> Result<Vec<String>, String> {
        match self {
            Dispatch::Local { pools, .. } => {
                let found = pools.drain_by_address(address);
                if found.is_empty() {
                    Err(format!("backend '{address}' not found in any pool"))
                } else {
                    Ok(found)
                }
            }
            Dispatch::Workers(w) => w.drain(address).await,
        }
    }

    /// Connections still open to a backend, summed over every worker.
    async fn connections_for(&self, address: &str) -> i64 {
        match self {
            Dispatch::Local { pools, .. } => pools.connections_for_address(address),
            Dispatch::Workers(w) => w.connections_for(address).await,
        }
    }

    /// Trigger a config reload. A worker reloads itself; the master signals
    /// itself and its supervision loop forwards SIGHUP to every worker.
    fn reload(&self) -> String {
        let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP);
        ControlResponse::ok(serde_json::json!({"message": "config reload triggered"}))
    }

    /// Cluster mode never forks, so only `Local` can hold a cluster handle.
    fn cluster(&self) -> Option<crate::cluster::ClusterHandle> {
        match self {
            Dispatch::Local { cluster, .. } => cluster.clone(),
            Dispatch::Workers(_) => None,
        }
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
    let mut line = String::new();
    reader.read_line(&mut line).await?;

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

    let cluster = dispatch.cluster();

    match request {
        ControlRequest::Status => {
            write_line(&mut writer, &dispatch.status().await).await?;
        }

        ControlRequest::BackendList { pool } => {
            write_line(&mut writer, &dispatch.backend_list(&pool).await).await?;
        }

        ControlRequest::BackendDrain { address, wait } => {
            let found = match dispatch.drain(&address).await {
                Ok(found) => found,
                Err(e) => {
                    write_line(&mut writer, &ControlResponse::err(e)).await?;
                    return Ok(());
                }
            };

            write_line(
                &mut writer,
                &ControlResponse::ok(serde_json::json!({
                    "pools": found,
                    "done": !wait,
                })),
            )
            .await?;

            if wait {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    let conns = dispatch.connections_for(&address).await;
                    let done = conns == 0;
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
            write_line(&mut writer, &dispatch.reload()).await?;
        }

        ControlRequest::ClusterStatus => {
            let resp = cmd_cluster_status(&cluster).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ClusterDemote => {
            let resp = cmd_cluster_demote(&cluster).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ClusterStepdown { force } => {
            let resp = cmd_cluster_stepdown(&cluster, force).await;
            write_line(&mut writer, &resp).await?;
        }

        ControlRequest::ConfigPush { yaml } => {
            let resp = cmd_config_push(&cluster, yaml).await;
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

fn cmd_status(pools: &PoolRegistry, started_at: Instant) -> String {
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

fn cmd_backend_list(pools: &PoolRegistry, pool_name: &str) -> String {
    if !pools.has_pool(pool_name) {
        return ControlResponse::err(format!("pool '{pool_name}' not found"));
    }
    let backends = pools.backends_for_pool(pool_name);
    ControlResponse::ok(serde_json::json!({
        "pool": pool_name,
        "backends": backends.iter().map(|b| backend_json(b)).collect::<Vec<_>>(),
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

async fn cmd_cluster_status(cluster: &Option<crate::cluster::ClusterHandle>) -> String {
    let Some(ch) = cluster else {
        return ControlResponse::err("not in cluster mode");
    };
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

async fn cmd_cluster_demote(cluster: &Option<crate::cluster::ClusterHandle>) -> String {
    let Some(ch) = cluster else {
        return ControlResponse::err("not in cluster mode");
    };
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

async fn cmd_cluster_stepdown(
    cluster: &Option<crate::cluster::ClusterHandle>,
    force: bool,
) -> String {
    let Some(ch) = cluster else {
        return ControlResponse::err("not in cluster mode");
    };
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

async fn cmd_config_push(cluster: &Option<crate::cluster::ClusterHandle>, yaml: String) -> String {
    let Some(ch) = cluster else {
        return ControlResponse::err("not in cluster mode");
    };
    let Some(raft) = ch.raft().await else {
        return ControlResponse::err("cluster not yet initialized");
    };
    match crate::cluster::push_config(&raft, yaml).await {
        Ok(()) => ControlResponse::ok(serde_json::json!({"message": "config committed to cluster"})),
        Err(e) => ControlResponse::err(e.to_string()),
    }
}
