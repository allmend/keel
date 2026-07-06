//! L4 TCP proxying.
//!
//! A listener with `tcp_pool` splices raw bytes between the client and a
//! backend selected from the named pool. Keel never inspects the stream —
//! TLS, if the client uses it, is negotiated end-to-end with the backend
//! (**passthrough** mode). `terminate` and `reencrypt` modes are planned.
//!
//! Backend selection, weights, health checks, drain, and connection counting
//! all come from the shared `PoolRegistry` — a TCP connection counts exactly
//! like an HTTP connection for `keel backend drain --wait`. The consistent
//! hashing key is the client IP:port, so a client keeps hitting the same
//! backend while the pool composition is stable.
//!
//! One access log entry is written per connection (not per request — there is
//! no request concept at L4) to `access_tcp_<pool>.log`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use pingora::apps::ServerApp;
use pingora::protocols::Stream;
use pingora::server::ShutdownWatch;
use tracing::{debug, warn};

use crate::access_log::{AccessLogger, TcpLogEntry};
use crate::backend::PoolRegistry;

pub struct TcpProxyApp {
    pub listener: String,
    pub pool: String,
    pub pools: Arc<PoolRegistry>,
    pub access_logger: Arc<AccessLogger>,
}

impl TcpProxyApp {
    fn log(
        &self,
        client_addr: Option<String>,
        backend_addr: Option<String>,
        bytes_in: u64,
        bytes_out: u64,
        started: std::time::Instant,
        error: Option<&str>,
    ) {
        self.access_logger.log_tcp(&TcpLogEntry {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            r#type: "tcp",
            client_addr,
            listener: self.listener.clone(),
            pool: self.pool.clone(),
            backend_addr,
            bytes_in,
            bytes_out,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: error.map(str::to_owned),
        });
    }
}

#[async_trait]
impl ServerApp for TcpProxyApp {
    async fn process_new(
        self: &Arc<Self>,
        mut session: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let started = std::time::Instant::now();
        let client_addr = session
            .get_socket_digest()
            .and_then(|d| d.peer_addr().map(|a| a.to_string()));

        // Client address doubles as the consistent-hash key (session affinity).
        let key = client_addr.clone().unwrap_or_default();
        let Some(backend) = self.pools.select(&self.pool, key.as_bytes()) else {
            warn!(pool = self.pool, listener = self.listener, "tcp: no healthy backend");
            self.log(client_addr, None, 0, 0, started, Some("no_backend"));
            return None;
        };

        let mut upstream = match tokio::net::TcpStream::connect(backend).await {
            Ok(s) => s,
            Err(e) => {
                warn!(pool = self.pool, backend = %backend, error = %e, "tcp: upstream connect failed");
                self.pools.release(&self.pool, backend);
                self.log(client_addr, Some(backend.to_string()), 0, 0, started, Some("upstream_connect"));
                return None;
            }
        };

        // Splice until either side closes, an I/O error occurs, or the server
        // shuts down (graceful shutdown closes L4 connections — there is no
        // in-flight request boundary to wait for).
        let mut shutdown = shutdown.clone();
        let (bytes_in, bytes_out, error) = tokio::select! {
            r = tokio::io::copy_bidirectional(&mut session, &mut upstream) => match r {
                Ok((client_to_backend, backend_to_client)) => (client_to_backend, backend_to_client, None),
                Err(e) => {
                    debug!(pool = self.pool, backend = %backend, error = %e, "tcp: connection ended with error");
                    (0, 0, Some("io"))
                }
            },
            _ = shutdown.changed() => (0, 0, Some("shutdown")),
        };

        self.pools.release(&self.pool, backend);
        self.log(client_addr, Some(backend.to_string()), bytes_in, bytes_out, started, error);
        None
    }
}
