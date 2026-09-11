//! L4 UDP load balancing.
//!
//! A listener with `udp_pool` forwards datagrams to a backend selected from
//! the named pool. UDP has no connection, so Keel tracks *flows*: one per
//! client `ip:port`, pinned to a backend for as long as datagrams keep
//! arriving. A flow ends when it has been idle for
//! `keel.udp_flow_timeout_seconds` (or on shutdown / a socket error) — there
//! is no close handshake to observe.
//!
//! Each flow owns a dedicated upstream socket connected to its backend, so
//! replies are matched to the client by socket, not by parsing payloads.
//! Flows register in the shared `PoolRegistry` counters exactly like TCP and
//! HTTP connections: `select` on the first datagram, `release` when the flow
//! ends. That is what makes `keel backend drain --wait` work for UDP — a
//! draining backend receives no new flows and moves to `removed` once its
//! last flow expires.
//!
//! This path does not go through Pingora's data plane. It runs as a
//! `BackgroundService` on raw Tokio sockets and mirrors the TCP L4 module for
//! metrics (`keel_udp_*`) and access logs (`access_udp_<pool>.log`, one entry
//! per flow).

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info};

use crate::access_log::{AccessLogger, UdpLogEntry};
use crate::backend::PoolRegistry;
use crate::metrics::UdpFlowCounters;

/// Largest payload a UDP datagram can carry. Receive buffers are this size so
/// no datagram is ever truncated.
const MAX_DATAGRAM: usize = 65_535;

/// How often idle flows are checked. A flow expires at most this long after
/// its timeout elapses.
const REAPER_INTERVAL: Duration = Duration::from_secs(1);

pub struct UdpProxyService {
    pub listener: String,
    pub pool: String,
    pub pools: Arc<PoolRegistry>,
    pub access_logger: Arc<AccessLogger>,
    pub flow_timeout: Duration,
    /// Socket bound by the root master before the privilege drop. `None`
    /// (unprivileged dev runs) means the service binds its own.
    pub socket: Option<std::net::UdpSocket>,
    /// Every datagram starts with a PROXY protocol v2 header (as an NLB
    /// sends for UDP); the client is the address it names.
    pub proxy_protocol: bool,
}

/// Per-flow accounting, shared between the receive loop (client → backend)
/// and the flow's reply task (backend → client). Lock-free so neither path
/// waits on the other; the reaper reads it from the receive loop's task.
pub struct FlowStats {
    started: Instant,
    /// Milliseconds since `started` of the most recent datagram in either
    /// direction. Stored as an integer so the reply task can bump it without
    /// a lock.
    last_activity_ms: AtomicU64,
    packets_in: AtomicU64,
    bytes_in: AtomicU64,
    packets_out: AtomicU64,
    bytes_out: AtomicU64,
    /// First error either side hit. A failed flow is closed on the next pass
    /// so the client's following datagram re-selects a backend.
    error: OnceLock<&'static str>,
}

impl FlowStats {
    pub fn new(now: Instant) -> Self {
        FlowStats {
            started: now,
            last_activity_ms: AtomicU64::new(0),
            packets_in: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            packets_out: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            error: OnceLock::new(),
        }
    }

    pub fn record_in(&self, now: Instant, bytes: usize) {
        self.packets_in.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch(now);
    }

    pub fn record_out(&self, now: Instant, bytes: usize) {
        self.packets_out.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch(now);
    }

    fn touch(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.started).as_millis() as u64;
        self.last_activity_ms.fetch_max(ms, Ordering::Relaxed);
    }

    /// Time since the last datagram in either direction (since open, if none).
    pub fn idle_for(&self, now: Instant) -> Duration {
        let last = self.started + Duration::from_millis(self.last_activity_ms.load(Ordering::Relaxed));
        now.saturating_duration_since(last)
    }

    pub fn is_expired(&self, now: Instant, timeout: Duration) -> bool {
        self.idle_for(now) >= timeout
    }

    /// Record the first error; later ones are ignored so the log entry names
    /// the cause, not a consequence.
    pub fn fail(&self, reason: &'static str) {
        let _ = self.error.set(reason);
    }

    pub fn error(&self) -> Option<&'static str> {
        self.error.get().copied()
    }

    pub fn duration_ms(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64() * 1000.0
    }
}

/// One client `ip:port` pinned to one backend. Dropped (with the reply task
/// aborted) when the flow ends.
struct Flow {
    backend: SocketAddr,
    upstream: Arc<UdpSocket>,
    stats: Arc<FlowStats>,
    counters: UdpFlowCounters,
    reply_task: tokio::task::JoinHandle<()>,
}

impl UdpProxyService {
    /// Open a flow for `client`: select a backend, connect an upstream socket,
    /// and start the reply task. `None` means the datagram is dropped; the
    /// reason is logged and counted.
    async fn open_flow(
        &self,
        downstream: &Arc<UdpSocket>,
        client: SocketAddr,
        reply_to: SocketAddr,
        now: Instant,
    ) -> Option<Flow> {
        // The client address doubles as the consistent-hash key, as for TCP.
        let key = client.to_string();
        let Some(backend) = self.pools.select(&self.pool, key.as_bytes()) else {
            // One line per dropped datagram is too chatty for warn — the
            // access log and keel_udp_errors_total carry the signal.
            debug!(pool = self.pool, listener = self.listener, "udp: no healthy backend");
            crate::metrics::record_udp_error(&self.pool, "no_backend");
            self.log(client, None, &FlowStats::new(now), Some("no_backend"));
            return None;
        };

        let upstream = async {
            let s = UdpSocket::bind(local_bind_addr(&backend)).await?;
            s.connect(backend).await?;
            Ok::<_, std::io::Error>(s)
        }
        .await;
        let upstream = match upstream {
            Ok(s) => Arc::new(s),
            Err(e) => {
                debug!(pool = self.pool, backend = %backend, error = %e, "udp: upstream socket failed");
                self.pools.release(&self.pool, backend);
                crate::metrics::record_udp_error(&self.pool, "upstream_bind");
                self.log(client, Some(backend), &FlowStats::new(now), Some("upstream_bind"));
                return None;
            }
        };

        let backend_label = backend.to_string();
        crate::metrics::record_udp_flow(&self.pool, &backend_label);
        let counters = crate::metrics::udp_flow_counters(&self.pool, &backend_label);
        let stats = Arc::new(FlowStats::new(now));
        let reply_task = tokio::spawn(reply_loop(
            Arc::clone(&upstream),
            Arc::clone(downstream),
            reply_to,
            Arc::clone(&stats),
            counters.clone(),
            self.pool.clone(),
            backend,
            Arc::clone(&self.pools),
        ));
        debug!(pool = self.pool, client = %client, backend = %backend, "udp: flow opened");
        Some(Flow { backend, upstream, stats, counters, reply_task })
    }

    /// Forward one datagram, opening a flow for an unknown client first.
    /// `client` keys the flow and is what logs and hashing see; `reply_to` is
    /// the socket peer replies go to. They differ behind a load balancer
    /// sending PROXY protocol headers: the client is in the header, the
    /// balancer is the peer.
    async fn handle_datagram(
        &self,
        downstream: &Arc<UdpSocket>,
        flows: &mut HashMap<SocketAddr, Flow>,
        client: SocketAddr,
        reply_to: SocketAddr,
        data: &[u8],
    ) {
        let now = Instant::now();

        // A flow whose reply task failed is closed here rather than left until
        // expiry, so this datagram re-selects a backend.
        if flows.get(&client).is_some_and(|f| f.stats.error().is_some()) {
            if let Some(flow) = flows.remove(&client) {
                self.close_flow(client, flow, None);
            }
        }
        if let std::collections::hash_map::Entry::Vacant(e) = flows.entry(client) {
            let Some(flow) = self.open_flow(downstream, client, reply_to, now).await else { return };
            e.insert(flow);
        }

        let flow = flows.get(&client).expect("flow inserted above");
        match flow.upstream.send(data).await {
            Ok(sent) => {
                flow.stats.record_in(now, sent);
                flow.counters.packet_in(sent);
            }
            Err(e) => {
                debug!(pool = self.pool, backend = %flow.backend, error = %e, "udp: upstream send failed");
                crate::metrics::record_udp_error(&self.pool, "upstream_send");
                flow.stats.fail("upstream_send");
                if let Some(flow) = flows.remove(&client) {
                    self.close_flow(client, flow, None);
                }
            }
        }
    }

    /// End a flow: release the backend (drives drain), write the access log
    /// entry. `error` overrides whatever the flow recorded (used for shutdown).
    fn close_flow(&self, client: SocketAddr, flow: Flow, error: Option<&str>) {
        flow.reply_task.abort();
        self.pools.release(&self.pool, flow.backend);
        let error = error.or_else(|| flow.stats.error());
        self.log(client, Some(flow.backend), &flow.stats, error);
        debug!(
            pool = self.pool,
            client = %client,
            backend = %flow.backend,
            error = error.unwrap_or("none"),
            "udp: flow closed"
        );
    }

    fn log(&self, client: SocketAddr, backend: Option<SocketAddr>, stats: &FlowStats, error: Option<&str>) {
        self.access_logger.log_udp(&UdpLogEntry {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            r#type: "udp",
            client_addr: client.to_string(),
            listener: self.listener.clone(),
            pool: self.pool.clone(),
            backend_addr: backend.map(|b| b.to_string()),
            bytes_in: stats.bytes_in.load(Ordering::Relaxed),
            bytes_out: stats.bytes_out.load(Ordering::Relaxed),
            packets_in: stats.packets_in.load(Ordering::Relaxed),
            packets_out: stats.packets_out.load(Ordering::Relaxed),
            duration_ms: stats.duration_ms(Instant::now()),
            error: error.map(str::to_owned),
        });
    }
}

#[async_trait]
impl BackgroundService for UdpProxyService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let bound = match self.socket.as_ref() {
            Some(s) => s.try_clone(),
            None => bind_reuseport(&self.listener),
        };
        let downstream = match bound.and_then(UdpSocket::from_std) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!(address = self.listener, pool = self.pool, error = %e, "udp: failed to bind");
                return;
            }
        };
        info!(
            address = self.listener,
            pool = self.pool,
            inherited = self.socket.is_some(),
            "udp: listening"
        );

        let mut flows: HashMap<SocketAddr, Flow> = HashMap::new();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut reaper = tokio::time::interval(REAPER_INTERVAL);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        // No request boundary at L4: flows are cut, and each
                        // still releases its backend and gets a log entry.
                        for (client, flow) in flows.drain() {
                            self.close_flow(client, flow, Some("shutdown"));
                        }
                        info!(address = self.listener, pool = self.pool, "udp: listener stopped");
                        return;
                    }
                }
                _ = reaper.tick() => {
                    let now = Instant::now();
                    let ended: Vec<SocketAddr> = flows
                        .iter()
                        .filter(|(_, f)| f.stats.error().is_some() || f.stats.is_expired(now, self.flow_timeout))
                        .map(|(client, _)| *client)
                        .collect();
                    for client in ended {
                        if let Some(flow) = flows.remove(&client) {
                            self.close_flow(client, flow, None);
                        }
                    }
                }
                received = downstream.recv_from(&mut buf) => {
                    match received {
                        Ok((n, peer)) => {
                            let client = peer;
                            let (client, data) = if self.proxy_protocol {
                                match crate::proxy_protocol::parse(&buf[..n]) {
                                    Ok(h) => (h.addresses.map(|(src, _)| src).unwrap_or(client), &buf[h.consumed..n]),
                                    Err(e) => {
                                        debug!(pool = self.pool, listener = self.listener, error = ?e, "udp: PROXY header rejected");
                                        crate::metrics::record_proxy_protocol_error(&self.listener);
                                        crate::metrics::record_udp_error(&self.pool, "proxy_protocol");
                                        continue;
                                    }
                                }
                            } else {
                                (client, &buf[..n])
                            };
                            self.handle_datagram(&downstream, &mut flows, client, peer, data).await
                        }
                        // Transient (e.g. ICMP surfaced on some platforms); the
                        // listener socket itself is not affected.
                        Err(e) => debug!(address = self.listener, error = %e, "udp: receive failed"),
                    }
                }
            }
        }
    }
}

/// Backend → client half of a flow. Ends on the first socket error; the flow
/// is then closed by the receive loop (on the next datagram or reaper pass).
#[allow(clippy::too_many_arguments)] // one call site; a struct would only add ceremony
async fn reply_loop(
    upstream: Arc<UdpSocket>,
    downstream: Arc<UdpSocket>,
    reply_to: SocketAddr,
    stats: Arc<FlowStats>,
    counters: UdpFlowCounters,
    pool: String,
    backend: SocketAddr,
    pools: Arc<PoolRegistry>,
) {
    loop {
        // Wait for readiness before allocating: an idle flow holds no buffer,
        // so memory scales with datagrams in flight, not with open flows.
        // `reserve` + `try_recv_buf` reads into uninitialised memory — no
        // 64 KiB memset per datagram. A pooled buffer is a later optimisation
        // (ROADMAP).
        let mut buf = bytes::BytesMut::new();
        let received = match upstream.readable().await {
            Ok(()) => {
                buf.reserve(MAX_DATAGRAM);
                upstream.try_recv_buf(&mut buf)
            }
            Err(e) => Err(e),
        };
        let n = match received {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                // A connected UDP socket surfaces ICMP port-unreachable here
                // (ECONNREFUSED): nothing listens at the backend address.
                debug!(pool, backend = %backend, error = %e, "udp: upstream receive failed");
                crate::metrics::record_udp_error(&pool, "upstream_recv");
                pools.report_failure(&pool, backend, "upstream_recv");
                stats.fail("upstream_recv");
                return;
            }
        };
        match downstream.send_to(&buf[..n], reply_to).await {
            Ok(sent) => {
                stats.record_out(Instant::now(), sent);
                counters.packet_out(sent);
                pools.report_success(&pool, backend);
            }
            Err(e) => {
                debug!(pool, reply_to = %reply_to, error = %e, "udp: reply send failed");
                crate::metrics::record_udp_error(&pool, "downstream_send");
                stats.fail("downstream_send");
                return;
            }
        }
    }
}

/// Bind with SO_REUSEPORT so several sockets can own the same port — one per
/// worker, bound by the master (see process::bind_listeners) or by each worker
/// itself in unprivileged runs. Linux spreads datagrams across the sockets of
/// the group by 4-tuple hash, so a client always reaches the same worker and
/// its flow table stays consistent.
pub fn bind_reuseport(addr: &str) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::ToSocketAddrs;

    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("no address for '{addr}'")))?;
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// Ephemeral local address in the backend's address family.
pub fn local_bind_addr(backend: &SocketAddr) -> SocketAddr {
    match backend {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn fresh_flow_expires_from_open_time() {
        let t0 = Instant::now();
        let stats = FlowStats::new(t0);
        assert!(!stats.is_expired(t0 + Duration::from_secs(29), TIMEOUT));
        assert!(stats.is_expired(t0 + Duration::from_secs(30), TIMEOUT));
    }

    #[test]
    fn activity_in_either_direction_defers_expiry() {
        let t0 = Instant::now();
        let stats = FlowStats::new(t0);
        stats.record_in(t0 + Duration::from_secs(20), 100);
        assert!(!stats.is_expired(t0 + Duration::from_secs(45), TIMEOUT));
        assert!(stats.is_expired(t0 + Duration::from_secs(50), TIMEOUT));

        stats.record_out(t0 + Duration::from_secs(40), 300);
        assert_eq!(stats.idle_for(t0 + Duration::from_secs(41)), Duration::from_secs(1));
        assert!(!stats.is_expired(t0 + Duration::from_secs(69), TIMEOUT));
        assert!(stats.is_expired(t0 + Duration::from_secs(70), TIMEOUT));
    }

    #[test]
    fn last_activity_never_moves_backwards() {
        // The reply task and receive loop stamp independently; an older
        // timestamp landing later must not shorten the flow's life.
        let t0 = Instant::now();
        let stats = FlowStats::new(t0);
        stats.record_in(t0 + Duration::from_secs(20), 1);
        stats.record_out(t0 + Duration::from_secs(10), 1);
        assert_eq!(stats.idle_for(t0 + Duration::from_secs(25)), Duration::from_secs(5));
    }

    #[test]
    fn counters_accumulate_per_direction() {
        let t0 = Instant::now();
        let stats = FlowStats::new(t0);
        stats.record_in(t0, 64);
        stats.record_in(t0, 36);
        stats.record_out(t0, 512);
        assert_eq!(stats.packets_in.load(Ordering::Relaxed), 2);
        assert_eq!(stats.bytes_in.load(Ordering::Relaxed), 100);
        assert_eq!(stats.packets_out.load(Ordering::Relaxed), 1);
        assert_eq!(stats.bytes_out.load(Ordering::Relaxed), 512);
        assert_eq!(stats.duration_ms(t0 + Duration::from_millis(1500)), 1500.0);
    }

    #[test]
    fn first_error_wins() {
        let stats = FlowStats::new(Instant::now());
        assert_eq!(stats.error(), None);
        stats.fail("upstream_recv");
        stats.fail("upstream_send");
        assert_eq!(stats.error(), Some("upstream_recv"));
    }

    #[test]
    fn upstream_socket_matches_backend_family() {
        let v4: SocketAddr = "10.0.0.1:53".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:53".parse().unwrap();
        assert_eq!(local_bind_addr(&v4), "0.0.0.0:0".parse::<SocketAddr>().unwrap());
        assert_eq!(local_bind_addr(&v6), "[::]:0".parse::<SocketAddr>().unwrap());
    }
}
