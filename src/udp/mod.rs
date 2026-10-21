//! L4 UDP load balancing.
//!
//! A `udp` listener forwards datagrams to a backend selected from
//! the named pool. UDP has no connection, so Keel tracks *flows*: one per
//! client `ip:port`, pinned to a backend for as long as datagrams keep
//! arriving. A flow ends when it has been idle for
//! `keel.udp_flow_timeout` (or on shutdown / a socket error) — there
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

pub mod handover;

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, warn};

use crate::access_log::{AccessLogger, UdpLogEntry};
use crate::backend::PoolRegistry;
use crate::metrics::UdpSeries;

/// Largest payload a UDP datagram can carry. Receive buffers are this size so
/// no datagram is ever truncated.
const MAX_DATAGRAM: usize = 65_535;

/// How often idle flows are checked. A flow expires at most this long after
/// its timeout elapses.
const REAPER_INTERVAL: Duration = Duration::from_secs(1);

pub struct UdpProxyService {
    /// The listener's name.
    pub listener: String,
    /// The `ip:port` this service receives on.
    pub address: String,
    pub pool: String,
    pub pools: Arc<PoolRegistry>,
    pub access_logger: Arc<AccessLogger>,
    pub flow_timeout: Duration,
    /// Most flows this worker holds at once; further clients are dropped
    /// until one expires.
    pub max_flows: usize,
    /// Socket bound by the root master before the privilege drop. `None`
    /// (unprivileged dev runs) means the service binds its own.
    pub socket: Option<std::net::UdpSocket>,
    /// Every datagram starts with a PROXY protocol v2 header (as an NLB
    /// sends for UDP); the client is the address it names.
    pub proxy_protocol: bool,
    /// The applied routing table, for this listener's access list.
    pub routing: Arc<arc_swap::ArcSwap<crate::vhost::RoutingTable>>,
    /// Where this worker's flows go when a newer generation replaces it,
    /// and where it takes them from when it replaced one.
    pub handover: PathBuf,
    /// Bound when this worker replaces one: wait for its flows first.
    pub predecessor: std::sync::Mutex<Option<std::os::unix::net::UnixListener>>,
}

/// How long a new worker waits for its predecessor's flows; it only waits
/// when there is one.
const HANDOVER_WAIT: Duration = Duration::from_secs(90);

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

/// Datagrams dropped without a flow (no backend, flow limit), per reason,
/// since the last flush. Logged as one entry per reason per second: a flood
/// against a dead pool would otherwise write one line per datagram.
#[derive(Default)]
struct Drops(HashMap<&'static str, Dropped>);

struct Dropped {
    datagrams: u64,
    bytes: u64,
    last_client: SocketAddr,
    since: Instant,
}

impl Drops {
    fn add(&mut self, reason: &'static str, client: SocketAddr, bytes: usize, now: Instant) {
        let d = self.0.entry(reason).or_insert(Dropped { datagrams: 0, bytes: 0, last_client: client, since: now });
        d.datagrams += 1;
        d.bytes += bytes as u64;
        d.last_client = client;
    }
}

/// One client `ip:port` pinned to one backend. Dropped (with the reply task
/// aborted) when the flow ends.
struct Flow {
    _open: crate::drain::Open,
    reply_to: SocketAddr,
    backend: SocketAddr,
    upstream: Arc<UdpSocket>,
    stats: Arc<FlowStats>,
    counters: UdpSeries,
    reply_task: tokio::task::JoinHandle<()>,
}

impl UdpProxyService {
    /// Open a flow for `client`: select a backend, connect an upstream socket,
    /// and start the reply task. `Err` means the datagram is dropped:
    /// `Some(reason)` for the caller to log with the other drops, `None` when
    /// it is logged already.
    async fn open_flow(
        &self,
        downstream: &Arc<UdpSocket>,
        client: SocketAddr,
        reply_to: SocketAddr,
        now: Instant,
    ) -> Result<Flow, Option<&'static str>> {
        // The client address doubles as the consistent-hash key, as for TCP.
        let key = client.to_string();
        let Some(backend) = self.pools.select(&self.pool, key.as_bytes()) else {
            // One line per dropped datagram is too chatty for warn — the
            // access log and keel_udp_errors_total carry the signal.
            debug!(pool = self.pool, listener = self.listener, "udp: no healthy backend");
            crate::metrics::record_udp_error(&self.pool, "no_backend");
            return Err(Some("no_backend"));
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
                return Err(None);
            }
        };

        // A selected backend is always in the registry.
        let counters = *self.pools.backend(&self.pool, backend).ok_or(None)?.series().udp();
        counters.flow();
        debug!(pool = self.pool, client = %client, backend = %backend, "udp: flow opened");
        Ok(self.flow(downstream, reply_to, backend, upstream, counters, now))
    }

    /// A flow over `upstream`, its reply task started.
    fn flow(&self, downstream: &Arc<UdpSocket>, reply_to: SocketAddr, backend: SocketAddr, upstream: Arc<UdpSocket>, counters: UdpSeries, now: Instant) -> Flow {
        let stats = Arc::new(FlowStats::new(now));
        let reply_task = tokio::spawn(reply_loop(
            Arc::clone(&upstream),
            Arc::clone(downstream),
            reply_to,
            Arc::clone(&stats),
            counters,
            self.pool.clone(),
            backend,
            Arc::clone(&self.pools),
        ));
        Flow { _open: crate::drain::Open::new(), reply_to, backend, upstream, stats, counters, reply_task }
    }

    /// The flows of the worker this one replaced, on their own sockets.
    async fn adopt(&self, downstream: &Arc<UdpSocket>, flows: &mut HashMap<SocketAddr, Flow>) {
        let Some(listener) = self.predecessor.lock().unwrap().take() else { return };
        let received = tokio::task::spawn_blocking(move || handover::receive(&listener, HANDOVER_WAIT)).await;
        let moved = match received {
            Ok(Ok(Some(moved))) => moved,
            Ok(Ok(None)) => {
                warn!(listener = self.listener, address = self.address, "udp: the replaced worker sent no flows");
                return;
            }
            Ok(Err(e)) => {
                error!(listener = self.listener, address = self.address, error = %e, "udp: flows of the replaced worker lost");
                return;
            }
            Err(_) => return,
        };
        let now = Instant::now();
        let n = moved.len();
        for (m, socket) in moved {
            let upstream = match socket.set_nonblocking(true).and_then(|()| UdpSocket::from_std(socket)) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    warn!(client = %m.client, error = %e, "udp: a handed-over flow could not be taken");
                    continue;
                }
            };
            let Some(entry) = self.pools.backend(&self.pool, m.backend) else { continue };
            self.pools.acquire(&self.pool, m.backend);
            let counters = *entry.series().udp();
            flows.insert(m.client, self.flow(downstream, m.reply_to, m.backend, upstream, counters, now));
        }
        info!(listener = self.listener, address = self.address, flows = n, "udp: took over the replaced worker's flows");
    }

    /// Replaced by a newer generation: its worker of the same index takes
    /// every flow, with its upstream socket. Nothing is logged as ended.
    async fn hand_over(&self, flows: &mut HashMap<SocketAddr, Flow>) {
        use std::os::fd::AsFd;
        let mut moving = Vec::with_capacity(flows.len());
        for (client, flow) in flows.drain() {
            flow.reply_task.abort();
            self.pools.release(&self.pool, flow.backend);
            match flow.upstream.as_fd().try_clone_to_owned() {
                Ok(fd) => moving.push((handover::Moved { client, reply_to: flow.reply_to, backend: flow.backend }, fd)),
                Err(e) => warn!(client = %client, error = %e, "udp: a flow could not be handed over"),
            }
        }
        let path = self.handover.clone();
        match tokio::task::spawn_blocking(move || handover::send(&path, moving)).await {
            Ok(Ok(n)) => info!(listener = self.listener, address = self.address, flows = n, "udp: flows handed to the new worker"),
            Ok(Err(e)) => warn!(listener = self.listener, address = self.address, error = %e, "udp: no new worker took the flows"),
            Err(_) => {}
        }
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
        drops: &mut Drops,
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
        // One lookup: the limit is checked in the vacant arm, so a datagram on an
        // established flow pays a single hash and the "inserted above" invariant
        // no longer has to hold across three separate lookups.
        let open = flows.len();
        let flow = match flows.entry(client) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // Checked when a flow opens: one already open runs until it
                // expires.
                if self.routing.load().listener_access(&self.listener).is_some_and(|l| !l.permits(client.ip())) {
                    crate::metrics::record_udp_error(&self.pool, "denied");
                    drops.add("denied", client, data.len(), now);
                    return;
                }
                if open >= self.max_flows {
                    // Opening one costs a socket and a task, and the worker's HTTP
                    // and TCP listeners share its descriptors. Existing flows keep
                    // working.
                    debug!(pool = self.pool, listener = self.listener, client = %client, flows = open, "udp: flow limit reached, datagram dropped");
                    crate::metrics::record_udp_error(&self.pool, "flow_limit");
                    drops.add("flow_limit", client, data.len(), now);
                    return;
                }
                match self.open_flow(downstream, client, reply_to, now).await {
                    Ok(flow) => e.insert(flow),
                    Err(reason) => {
                        if let Some(reason) = reason {
                            drops.add(reason, client, data.len(), now);
                        }
                        return;
                    }
                }
            }
        };
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

    /// One entry per drop reason: `packets_in` and `bytes_in` are what was
    /// dropped since the last one, `client_addr` the last client.
    fn log_drops(&self, drops: &mut Drops) {
        let now = Instant::now();
        for (reason, d) in drops.0.drain() {
            self.access_logger.log_udp(&UdpLogEntry {
                timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                r#type: "udp",
                client_addr: d.last_client.to_string(),
                listener: self.listener.clone(),
                pool: self.pool.clone(),
                backend_addr: None,
                bytes_in: d.bytes,
                bytes_out: 0,
                packets_in: d.datagrams,
                packets_out: 0,
                duration_ms: now.saturating_duration_since(d.since).as_secs_f64() * 1000.0,
                error: Some(reason.to_owned()),
            });
        }
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
            None => bind_reuseport(&self.address),
        };
        let downstream = match bound.and_then(UdpSocket::from_std) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!(listener = self.listener, address = self.address, pool = self.pool, error = %e, "udp: failed to bind");
                return;
            }
        };
        info!(
            listener = self.listener,
            address = self.address,
            pool = self.pool,
            inherited = self.socket.is_some(),
            "udp: listening"
        );

        let mut flows: HashMap<SocketAddr, Flow> = HashMap::new();
        // Datagrams wait in the shared socket meanwhile.
        self.adopt(&downstream, &mut flows).await;
        let mut drops = Drops::default();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut reaper = tokio::time::interval(REAPER_INTERVAL);
        reaper.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        if crate::drain::draining() {
                            self.hand_over(&mut flows).await;
                        }
                        // Keel stopping: no request boundary at L4, flows are
                        // cut, each still releasing its backend and logged.
                        for (client, flow) in flows.drain() {
                            self.close_flow(client, flow, Some("shutdown"));
                        }
                        self.log_drops(&mut drops);
                        info!(listener = self.listener, address = self.address, pool = self.pool, "udp: listener stopped");
                        return;
                    }
                }
                _ = reaper.tick() => {
                    self.log_drops(&mut drops);
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
                            self.handle_datagram(&downstream, &mut flows, &mut drops, client, peer, data).await
                        }
                        // Transient (e.g. ICMP surfaced on some platforms); the
                        // listener socket itself is not affected.
                        Err(e) => debug!(listener = self.listener, address = self.address, error = %e, "udp: receive failed"),
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
    counters: UdpSeries,
    pool: String,
    backend: SocketAddr,
    pools: Arc<PoolRegistry>,
) {
    loop {
        // Wait for readiness before allocating: an idle flow holds no buffer,
        // so memory scales with datagrams in flight, not with open flows.
        // `reserve` + `try_recv_buf` reads into uninitialised memory — no
        // 64 KiB memset per datagram.
        // TODO: reuse reply buffers from a per-worker pool.
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

    let addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("'{addr}' is not an ip:port")))?;
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
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
