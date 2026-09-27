//! A worker's own control socket, `workers/worker-<index>.sock`. Only the
//! control worker talks to it: for the worker's view of its backends, to
//! drain on this node at once, and to hand over every applied config.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use keel_control::ControlResponse;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info};

use crate::backend::PoolRegistry;
use crate::control::fanout::{Applied, WorkerRequest, MAX_WORKER_LINE};

/// Applies what the control worker sends: the worker's routing, pools,
/// certificates and drains.
pub trait Apply: Send + Sync {
    fn apply(&self, applied: &Applied) -> Result<(), String>;
}

pub struct WorkerSocket {
    pub path: String,
    pub pools: Arc<PoolRegistry>,
    pub applier: Arc<dyn Apply>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for WorkerSocket {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::remove_file(&self.path);
        let listener = match UnixListener::bind(&self.path) {
            Ok(l) => l,
            Err(e) => {
                error!(path = self.path, error = %e, "worker: cannot bind control socket");
                return;
            }
        };
        // The control worker reaches it through the worker group; nobody else.
        if let Err(e) = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o660)) {
            error!(path = self.path, error = %e, "worker: cannot restrict control socket");
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        info!(path = self.path, "worker: control socket ready");
        let started_at = Instant::now();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                result = listener.accept() => {
                    let Ok((stream, _)) = result else { continue };
                    let pools = Arc::clone(&self.pools);
                    let applier = Arc::clone(&self.applier);
                    tokio::spawn(async move {
                        let (reader, mut writer) = tokio::io::split(stream);
                        let answer = match crate::control::read_line_capped(&mut BufReader::new(reader), MAX_WORKER_LINE).await {
                            Ok(line) => match serde_json::from_str::<WorkerRequest>(line.trim()) {
                                Ok(request) => answer(request, &pools, applier.as_ref(), started_at),
                                Err(e) => ControlResponse::err(format!("invalid request: {e}")),
                            },
                            Err(e) => ControlResponse::err(format!("invalid request: {e}")),
                        };
                        let _ = writer.write_all(answer.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                        let _ = writer.flush().await;
                    });
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn answer(request: WorkerRequest, pools: &PoolRegistry, applier: &dyn Apply, started_at: Instant) -> String {
    match request {
        WorkerRequest::Status => crate::control::cmd_status(pools, started_at),
        WorkerRequest::BackendList { pool } => crate::control::cmd_backend_list(pools, &pool),
        WorkerRequest::Drain { address } => {
            let found = pools.drain_by_address(&address);
            if found.is_empty() {
                ControlResponse::err(format!("backend '{address}' not found in any pool"))
            } else {
                ControlResponse::ok(serde_json::json!({ "pools": found }))
            }
        }
        WorkerRequest::Apply(applied) => match applier.apply(&applied) {
            Ok(()) => ControlResponse::ok(serde_json::json!({ "version": applied.version })),
            Err(e) => ControlResponse::err(e),
        },
    }
}
