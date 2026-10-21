//! Active health checks, run once per node by the control worker.
//!
//! One `HealthService` per pool with a `health_check` block probes every
//! backend on an interval. The `HealthTable` applies the thresholds and
//! writes each backend's verdict into the shared memory region, where every
//! worker reads it at selection and `keel_backend_healthy` is rendered from
//! it. A worker started later reads the current verdict; the verdicts stay
//! while the control worker is down, and a restarted one continues from
//! them. The table keeps the failure reasons for `keel status`.
//!
//! Probes are protocol checks behind one small trait. `tcp` and `udp` prove
//! the port answers (or, for UDP, that it does not refuse); `http`, `dns`,
//! and `ntp` talk to the service itself; `tls` completes a handshake and
//! can watch certificate expiry; `icmp` only reaches the host.
//!
//! The first round starts within a second of startup and every round is
//! jittered by ±10%, so pools drift apart.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::net::{TcpStream, UdpSocket};
use tracing::{debug, info, warn};

use crate::config::{Config, HealthCheck, ProbeConfig};

mod dns;
pub mod icmp;
mod ntp;
mod tls;

/// `Err` carries a short reason shown in `keel status`.
pub type ProbeResult = Result<(), String>;

#[async_trait]
pub trait Probe: Send + Sync {
    async fn probe(&self, target: SocketAddr) -> ProbeResult;

    /// A probe that treats "no answer within the timeout" as success (UDP)
    /// applies the timeout itself; the scheduler must not fail it first.
    fn owns_timeout(&self) -> bool {
        false
    }
}

pub struct HealthService {
    pool: String,
    /// (traffic address, probed address) — they differ when `port` is set.
    targets: Vec<(SocketAddr, SocketAddr)>,
    probe: Arc<dyn Probe>,
    interval: Duration,
    timeout: Duration,
    healthy_after: u32,
    unhealthy_after: u32,
    table: Arc<HealthTable>,
}

/// The health of every checked backend on this node.
#[derive(Default)]
pub struct HealthTable {
    backends: std::sync::RwLock<HashMap<(String, SocketAddr), Arc<Checked>>>,
}

/// One backend's probe results and verdict.
struct Checked {
    series: crate::metrics::BackendSeries,
    /// No verdict yet: the first probe decides.
    initializing: AtomicBool,
    healthy: AtomicBool,
    consecutive_ok: AtomicU32,
    consecutive_fail: AtomicU32,
    /// Why the last probe failed, until the next success.
    reason: Mutex<Option<String>>,
}

impl Checked {
    fn new(pool: &str, addr: SocketAddr) -> Self {
        let series = crate::metrics::BackendSeries::new(pool, &addr.to_string());
        // A restarted control worker continues from the verdict it left; a
        // new backend has none, and gets no traffic until its first probe.
        let verdict = series.verdict();
        Checked {
            series,
            initializing: AtomicBool::new(verdict.is_none()),
            healthy: AtomicBool::new(verdict.unwrap_or(false)),
            consecutive_ok: AtomicU32::new(0),
            consecutive_fail: AtomicU32::new(0),
            reason: Mutex::new(None),
        }
    }

    /// A changed check counts its thresholds afresh; the verdict stays.
    fn reset_streaks(&self) {
        self.consecutive_ok.store(0, Ordering::Relaxed);
        self.consecutive_fail.store(0, Ordering::Relaxed);
    }

    /// Apply one probe result with the pool's thresholds. Returns
    /// `Some(new_state)` when the backend flipped.
    fn observe(&self, ok: bool, reason: Option<&str>, healthy_after: u32, unhealthy_after: u32) -> Option<bool> {
        if self.initializing.swap(false, Ordering::Relaxed) {
            *self.reason.lock().unwrap() = reason.map(str::to_owned);
            let counter = if ok { &self.consecutive_ok } else { &self.consecutive_fail };
            counter.store(1, Ordering::Relaxed);
            self.healthy.store(ok, Ordering::Relaxed);
            return Some(ok);
        }
        if ok {
            self.consecutive_fail.store(0, Ordering::Relaxed);
            *self.reason.lock().unwrap() = None;
            let n = self.consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.healthy.load(Ordering::Relaxed) && n >= healthy_after.max(1) {
                self.healthy.store(true, Ordering::Relaxed);
                return Some(true);
            }
        } else {
            self.consecutive_ok.store(0, Ordering::Relaxed);
            *self.reason.lock().unwrap() = reason.map(str::to_owned);
            let n = self.consecutive_fail.fetch_add(1, Ordering::Relaxed) + 1;
            if self.healthy.load(Ordering::Relaxed) && n >= unhealthy_after.max(1) {
                self.healthy.store(false, Ordering::Relaxed);
                return Some(false);
            }
        }
        None
    }
}

impl HealthTable {
    /// Record one probe result; a flip is published to the workers and logged.
    fn observe(&self, pool: &str, addr: SocketAddr, result: &ProbeResult, healthy_after: u32, unhealthy_after: u32) {
        let Some(c) = self.checked(pool, addr) else { return };
        let reason = result.as_ref().err().map(String::as_str);
        match c.observe(result.is_ok(), reason, healthy_after, unhealthy_after) {
            Some(true) => {
                c.series.set_healthy(true);
                info!(pool, backend = %addr, "health: backend healthy");
            }
            Some(false) => {
                c.series.set_healthy(false);
                warn!(pool, backend = %addr, reason = reason.unwrap_or("unknown"), "health: backend unhealthy");
            }
            None => {}
        }
    }

    /// Why the last probe of a backend failed, for `keel status`.
    pub fn reason(&self, pool: &str, address: &str) -> Option<String> {
        let addr: SocketAddr = address.parse().ok()?;
        self.checked(pool, addr)?.reason.lock().unwrap().clone()
    }
}

impl HealthTable {
    fn checked(&self, pool: &str, addr: SocketAddr) -> Option<Arc<Checked>> {
        self.backends.read().unwrap().get(&(pool.to_owned(), addr)).cloned()
    }
}

/// The node's prober: one task per pool with a health check, following the
/// applied config. A pool whose check and backends did not change keeps its
/// task; a changed one restarts with the new settings, its backends keeping
/// their verdict; a new backend starts without one.
pub struct Supervisor {
    pub table: Arc<HealthTable>,
    /// Pool → (check and backends it runs with, its task).
    running: HashMap<String, (String, tokio::task::JoinHandle<()>)>,
    shutdown: ShutdownWatch,
}

impl Supervisor {
    pub fn new(shutdown: ShutdownWatch) -> Self {
        Supervisor { table: Arc::default(), running: HashMap::new(), shutdown }
    }

    /// Run the checks `cfg` asks for. Call within a tokio runtime.
    pub fn apply(&mut self, cfg: &Config) {
        let mut wanted = HashMap::new();
        for (name, pool) in &cfg.pools {
            let Some(hc) = &pool.health_check else { continue };
            // Validated as ip:port.
            let addrs: Vec<SocketAddr> = pool.backends.iter().filter_map(|b| b.address.parse().ok()).collect();
            let recipe = serde_json::to_string(&(hc, &addrs)).unwrap_or_default();
            wanted.insert(name.clone(), (recipe, hc.clone(), addrs));
        }
        self.running.retain(|pool, (recipe, task)| {
            let keep = wanted.get(pool).is_some_and(|(r, _, _)| r == recipe);
            if !keep {
                task.abort();
                info!(pool, "health: checks stopped");
            }
            keep
        });
        {
            let mut backends = self.table.backends.write().unwrap();
            backends.retain(|(pool, addr), _| wanted.get(pool).is_some_and(|(_, _, addrs)| addrs.contains(addr)));
            for (pool, (_, _, addrs)) in &wanted {
                for addr in addrs {
                    let c = backends.entry((pool.clone(), *addr)).or_insert_with(|| Arc::new(Checked::new(pool, *addr)));
                    if !self.running.contains_key(pool) {
                        c.reset_streaks();
                    }
                }
            }
        }
        for (pool, (recipe, hc, addrs)) in wanted {
            if self.running.contains_key(&pool) {
                continue;
            }
            let service = HealthService::build(&hc, &pool, &addrs, Arc::clone(&self.table));
            let shutdown = self.shutdown.clone();
            let task = tokio::spawn(async move { pingora::services::background::BackgroundService::start(&service, shutdown).await });
            self.running.insert(pool, (recipe, task));
        }
    }
}

impl HealthService {
    fn build(cfg: &HealthCheck, pool: &str, addrs: &[SocketAddr], table: Arc<HealthTable>) -> Self {
        let timeout = cfg.timeout;
        let probe: Arc<dyn Probe> = match &cfg.probe {
            ProbeConfig::Tcp => Arc::new(TcpProbe),
            ProbeConfig::Udp => Arc::new(UdpProbe { timeout }),
            ProbeConfig::Http { path, host, tls, expect_status, expect_body } => Arc::new(HttpProbe {
                connector: pingora::connectors::http::Connector::new(None),
                path: path.clone(),
                host: host.clone(),
                tls: *tls,
                expect_status: expect_status.clone(),
                expect_body: expect_body.clone(),
                timeout,
            }),
            ProbeConfig::Dns { query, record, transport, expect } => Arc::new(dns::DnsProbe::new(
                query,
                *record,
                *transport,
                // Validated as an address of the record's family at load time.
                expect.as_deref().and_then(|e| e.parse().ok()),
            )),
            ProbeConfig::Ntp => Arc::new(ntp::NtpProbe),
            ProbeConfig::Icmp => Arc::new(icmp::IcmpProbe::new(timeout)),
            ProbeConfig::Tls { sni, min_valid } => Arc::new(tls::TlsProbe::new(sni.clone(), *min_valid)),
        };
        let targets = addrs
            .iter()
            .map(|&traffic| {
                let mut probed = traffic;
                if let Some(port) = cfg.port {
                    probed.set_port(port);
                }
                (traffic, probed)
            })
            .collect();
        HealthService {
            pool: pool.to_owned(),
            targets,
            probe,
            interval: cfg.interval,
            timeout,
            healthy_after: cfg.healthy_threshold,
            unhealthy_after: cfg.unhealthy_threshold,
            table,
        }
    }

    /// Probe every backend concurrently and record the results.
    async fn run_round(&self) {
        let handles: Vec<_> = self
            .targets
            .iter()
            .map(|&(traffic, probed)| {
                let probe = Arc::clone(&self.probe);
                let timeout = self.timeout;
                tokio::spawn(async move {
                    let result = if probe.owns_timeout() {
                        probe.probe(probed).await
                    } else {
                        match tokio::time::timeout(timeout, probe.probe(probed)).await {
                            Ok(r) => r,
                            Err(_) => Err(format!("timeout after {}ms", timeout.as_millis())),
                        }
                    };
                    (traffic, result)
                })
            })
            .collect();
        for h in handles {
            let Ok((addr, result)) = h.await else { continue };
            debug!(pool = self.pool, backend = %addr, ok = result.is_ok(), "health: probe");
            self.table.observe(&self.pool, addr, &result, self.healthy_after, self.unhealthy_after);
        }
    }
}

#[async_trait]
impl BackgroundService for HealthService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        info!(
            pool = self.pool,
            backends = self.targets.len(),
            interval_ms = self.interval.as_millis() as u64,
            "health: checks started"
        );
        let mut rng = Rng::seeded(&self.pool);
        // First round within a second so dead backends are found quickly;
        // the random offset keeps pools from probing in lockstep.
        let mut delay = Duration::from_millis(rng.below(1000));
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tokio::time::sleep(delay) => {
                    self.run_round().await;
                    // ±10% jitter.
                    let ms = self.interval.as_millis() as u64;
                    delay = Duration::from_millis(ms * 9 / 10 + rng.below(ms / 5 + 1));
                }
            }
        }
    }
}

// Probes

/// TCP connect succeeds.
pub struct TcpProbe;

#[async_trait]
impl Probe for TcpProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        TcpStream::connect(target).await.map(drop).map_err(|e| io_reason(&e))
    }
}

/// Send an empty datagram and watch for ICMP port-unreachable. UDP has no
/// handshake, so silence within the timeout counts as healthy: the probe can
/// only prove a port *closed*, never open. A firewall that drops ICMP hides
/// a dead port from it.
pub struct UdpProbe {
    timeout: Duration,
}

#[async_trait]
impl Probe for UdpProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        let sock = UdpSocket::bind(unspecified(&target)).await.map_err(|e| io_reason(&e))?;
        sock.connect(target).await.map_err(|e| io_reason(&e))?;
        sock.send(&[]).await.map_err(|e| io_reason(&e))?;
        let mut buf = [0u8; 64];
        match tokio::time::timeout(self.timeout, sock.recv(&mut buf)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                Err("port unreachable".to_owned())
            }
            Ok(Err(e)) => Err(io_reason(&e)),
            Err(_) => Ok(()), // no ICMP error within the timeout
        }
    }

    fn owns_timeout(&self) -> bool {
        true
    }
}

/// HTTP GET through Pingora's connector. Success is a 2xx status unless
/// `expect_status` lists the acceptable codes; `expect_body` additionally
/// requires the substring in the first 64 KiB of the body.
pub struct HttpProbe {
    connector: pingora::connectors::http::Connector,
    path: String,
    host: Option<String>,
    tls: bool,
    expect_status: Vec<crate::config::StatusRange>,
    expect_body: Option<String>,
    timeout: Duration,
}

const BODY_CAP: usize = 64 * 1024;

#[async_trait]
impl Probe for HttpProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        use pingora::http::RequestHeader;
        use pingora::upstreams::peer::HttpPeer;

        let host = self.host.clone().unwrap_or_else(|| target.to_string());
        let mut peer = HttpPeer::new(target, self.tls, host.clone());
        peer.options.connection_timeout = Some(self.timeout);
        peer.options.read_timeout = Some(self.timeout);
        // Liveness, not identity: backends commonly present internal or
        // self-signed certificates to the proxy.
        peer.options.verify_cert = false;
        peer.options.verify_hostname = false;

        let (mut session, _) = self
            .connector
            .get_http_session(&peer)
            .await
            .map_err(|e| format!("connect: {}", e.etype().as_str()))?;

        let mut req = RequestHeader::build("GET", self.path.as_bytes(), None)
            .map_err(|e| format!("request: {}", e.etype().as_str()))?;
        let _ = req.insert_header("Host", host);
        let _ = req.insert_header("User-Agent", "keel-health");
        session
            .write_request_header(Box::new(req))
            .await
            .map_err(|e| format!("write: {}", e.etype().as_str()))?;
        session
            .finish_request_body()
            .await
            .map_err(|e| format!("write: {}", e.etype().as_str()))?;
        session.set_read_timeout(Some(self.timeout));
        session
            .read_response_header()
            .await
            .map_err(|e| format!("read: {}", e.etype().as_str()))?;

        let status = session.response_header().map(|r| r.status.as_u16()).unwrap_or(0);
        let accepted = if self.expect_status.is_empty() {
            (200..400).contains(&status)
        } else {
            self.expect_status.iter().any(|r| r.contains(status))
        };
        if !accepted {
            return Err(format!("status {status}"));
        }

        // Always drain the body so the connection ends cleanly.
        let mut body = Vec::new();
        while let Some(chunk) = session
            .read_response_body()
            .await
            .map_err(|e| format!("read body: {}", e.etype().as_str()))?
        {
            if self.expect_body.is_some() && body.len() < BODY_CAP {
                body.extend_from_slice(&chunk);
            }
        }
        if let Some(needle) = &self.expect_body {
            if !String::from_utf8_lossy(&body).contains(needle.as_str()) {
                return Err(format!("body lacks {needle:?}"));
            }
        }
        Ok(())
    }
}

// Helpers

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_owned(),
        std::io::ErrorKind::TimedOut => "timeout".to_owned(),
        std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable => {
            "unreachable".to_owned()
        }
        _ => e.to_string(),
    }
}

pub fn unspecified(target: &SocketAddr) -> SocketAddr {
    match target {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    }
}

/// xorshift64*, seeded per pool and process. Jitter needs no more than this
/// and it keeps `rand` out of the dependency tree.
pub struct Rng(u64);

impl Rng {
    pub fn seeded(salt: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        let mut seed = nanos ^ ((std::process::id() as u64) << 32);
        for b in salt.bytes() {
            seed = seed.wrapping_mul(1_099_511_628_211) ^ b as u64;
        }
        Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `0..n` (`0` when `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_flip_on_thresholds_and_are_published() {
        let addr: SocketAddr = "127.0.0.7:1".parse().unwrap();
        let table = HealthTable::default();
        table.backends.write().unwrap().insert(("hc".to_owned(), addr), Arc::new(Checked::new("hc", addr)));
        let worker_view = crate::metrics::BackendSeries::new("hc", "127.0.0.7:1");
        assert_eq!(worker_view.verdict(), None, "no verdict before the first probe");
        let fail: ProbeResult = Err("refused".into());
        // The first probe decides at once, either way.
        table.observe("hc", addr, &Ok(()), 2, 3);
        assert_eq!(worker_view.verdict(), Some(true));
        table.observe("hc", addr, &fail, 2, 3);
        table.observe("hc", addr, &fail, 2, 3);
        assert_eq!(worker_view.verdict(), Some(true), "below threshold");
        table.observe("hc", addr, &fail, 2, 3);
        assert_eq!(worker_view.verdict(), Some(false));
        assert_eq!(table.reason("hc", "127.0.0.7:1").as_deref(), Some("refused"));
        table.observe("hc", addr, &Ok(()), 2, 3);
        assert_eq!(worker_view.verdict(), Some(false));
        table.observe("hc", addr, &Ok(()), 2, 3);
        assert_eq!(worker_view.verdict(), Some(true));
        assert_eq!(table.reason("hc", "127.0.0.7:1"), None);
        let other: SocketAddr = "127.0.0.7:2".parse().unwrap();
        table.backends.write().unwrap().insert(("hc".to_owned(), other), Arc::new(Checked::new("hc", other)));
        table.observe("hc", other, &fail, 2, 3);
        assert_eq!(crate::metrics::BackendSeries::new("hc", "127.0.0.7:2").verdict(), Some(false), "a first failure is unhealthy at once");
        // A restarted prober continues from the verdict it left.
        worker_view.set_healthy(false);
        assert!(!Checked::new("hc", addr).healthy.load(Ordering::Relaxed));
    }


    #[tokio::test]
    async fn the_supervisor_follows_the_config() {
        let cfg = |backends: &str, path: &str| {
            crate::config::parse(&format!("pool \"sv\" {{\n  health_check \"http\" {{\n    path = \"{path}\"\n  }}\n\n{backends}}}\n")).unwrap()
        };
        let (_tx, shutdown) = tokio::sync::watch::channel(false);
        let mut sv = Supervisor::new(shutdown);
        let a: SocketAddr = "127.0.3.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.3.2:1".parse().unwrap();
        sv.apply(&cfg("  backend \"127.0.3.1:1\" {}\n", "/"));
        sv.table.observe("sv", a, &Err("refused".into()), 1, 1);
        let task = sv.running["sv"].1.id();
        sv.apply(&cfg("  backend \"127.0.3.1:1\" {}\n", "/"));
        assert_eq!(sv.running["sv"].1.id(), task, "unchanged: the same task");
        sv.apply(&cfg("  backend \"127.0.3.1:1\" {}\n  backend \"127.0.3.2:1\" {}\n", "/health"));
        assert_ne!(sv.running["sv"].1.id(), task, "changed: a new task");
        assert_eq!(sv.table.checked("sv", a).map(|c| c.healthy.load(Ordering::Relaxed)), Some(false), "a's verdict kept");
        assert!(sv.table.checked("sv", b).is_some_and(|c| c.initializing.load(Ordering::Relaxed)), "b starts without one");
        sv.apply(&crate::config::parse("pool \"sv\" {\n  backend \"127.0.3.1:1\" {}\n}\n").unwrap());
        assert!(sv.running.is_empty() && sv.table.checked("sv", a).is_none(), "check removed: stopped");
    }

    #[test]
    fn rng_stays_in_range_and_varies() {
        let mut r = Rng::seeded("pool");
        let vals: Vec<u64> = (0..100).map(|_| r.below(10)).collect();
        assert!(vals.iter().all(|v| *v < 10));
        assert!(vals.iter().any(|v| *v != vals[0]));
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn unspecified_matches_family() {
        assert_eq!(unspecified(&"10.0.0.1:53".parse().unwrap()), "0.0.0.0:0".parse::<SocketAddr>().unwrap());
        assert_eq!(unspecified(&"[::1]:53".parse().unwrap()), "[::]:0".parse::<SocketAddr>().unwrap());
    }

    #[tokio::test]
    async fn tcp_probe_reports_refused_and_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        assert_eq!(TcpProbe.probe(open).await, Ok(()));
        drop(listener);
        // The port is closed now; the OS refuses the connection.
        assert_eq!(TcpProbe.probe(open).await, Err("connection refused".to_owned()));
    }

    #[tokio::test]
    async fn udp_probe_detects_closed_port_and_accepts_silence() {
        let bound = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = bound.local_addr().unwrap();
        let probe = UdpProbe { timeout: Duration::from_millis(300) };
        // Open (silent) port: no ICMP, healthy by absence of evidence.
        assert_eq!(probe.probe(addr).await, Ok(()));
        drop(bound);
        // Closed port on loopback: the kernel answers with port unreachable.
        assert_eq!(probe.probe(addr).await, Err("port unreachable".to_owned()));
    }
}
