use crate::backend::{BackendStatus, PoolRegistry, DRAIN_ACTIVE, DRAIN_DRAINING, DRAIN_REMOVED};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info};

// Protocol types

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

#[derive(Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    fn ok(data: impl Serialize) -> String {
        let val = serde_json::to_value(data).unwrap_or_default();
        let r = ControlResponse { ok: true, data: Some(val), error: None };
        serde_json::to_string(&r).unwrap_or_default()
    }

    fn err(msg: impl Into<String>) -> String {
        let r = ControlResponse { ok: false, data: None, error: Some(msg.into()) };
        serde_json::to_string(&r).unwrap_or_default()
    }
}

// Server

pub struct ControlServer {
    pub socket_path: String,
    pub pools: Arc<PoolRegistry>,
    pub started_at: Instant,
    pub cluster: Option<crate::cluster::ClusterHandle>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ControlServer {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
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
                            let pools = Arc::clone(&self.pools);
                            let started_at = self.started_at;
                            let cluster = self.cluster.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, pools, started_at, cluster).await {
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

// Connection handler

async fn handle_connection(
    stream: tokio::net::UnixStream,
    pools: Arc<PoolRegistry>,
    started_at: Instant,
    cluster: Option<crate::cluster::ClusterHandle>,
) -> anyhow::Result<()> {
    let (reader_half, mut writer) = stream.into_split();
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

    match request {
        ControlRequest::Status => {
            write_line(&mut writer, &cmd_status(&pools, started_at)).await?;
        }

        ControlRequest::BackendList { pool } => {
            write_line(&mut writer, &cmd_backend_list(&pools, &pool)).await?;
        }

        ControlRequest::BackendDrain { address, wait } => {
            let found = pools.drain_by_address(&address);
            if found.is_empty() {
                write_line(
                    &mut writer,
                    &ControlResponse::err(format!("backend '{address}' not found in any pool")),
                )
                .await?;
                return Ok(());
            }

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
                    let conns = pools.connections_for_address(&address);
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
            let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP);
            write_line(
                &mut writer,
                &ControlResponse::ok(serde_json::json!({"message": "config reload triggered"})),
            )
            .await?;
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

async fn write_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    line: &str,
) -> anyhow::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
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
    serde_json::json!({
        "address": b.address,
        "state": state,
        "connections": b.connections,
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
