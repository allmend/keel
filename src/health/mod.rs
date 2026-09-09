//! Active health checks.
//!
//! One `HealthService` per pool with a `health_check` block probes every
//! backend on an interval and feeds the results into the pool registry's
//! health state (`PoolRegistry::observe_health`), which is what backend
//! selection, `keel status`, and `keel_backend_healthy` read. Pingora's own
//! health table is not used: it is private, so the passive detector and the
//! least-connections pool could not share it.
//!
//! Probes are protocol checks behind one small trait. `tcp` and `udp` prove
//! the port answers (or, for UDP, that it does not refuse); `http`, `dns`,
//! and `ntp` talk to the service itself; `tls` completes a handshake and
//! can watch certificate expiry; `icmp` only reaches the host.
//!
//! Every worker process runs its own checks, so a backend receives one probe
//! per worker per interval. The first round starts within a second of
//! startup and every round is jittered by ±10% so workers drift apart.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::net::{TcpStream, UdpSocket};
use tracing::{debug, info};

use crate::backend::PoolRegistry;
use crate::config::{HealthCheck, ProbeConfig};

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
    pools: Arc<PoolRegistry>,
}

impl HealthService {
    /// `addrs` are the pool's resolved backend addresses (the drain keys).
    pub fn build(
        cfg: &HealthCheck,
        pool: &str,
        addrs: &[String],
        pools: Arc<PoolRegistry>,
    ) -> anyhow::Result<Self> {
        let timeout = parse_duration(&cfg.timeout);
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
            ProbeConfig::Tls { sni, min_days_valid } => Arc::new(tls::TlsProbe::new(sni.clone(), *min_days_valid)),
        };
        let targets = addrs
            .iter()
            .map(|a| {
                let traffic: SocketAddr = a.parse()?;
                let mut probed = traffic;
                if let Some(port) = cfg.port {
                    probed.set_port(port);
                }
                Ok((traffic, probed))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(HealthService {
            pool: pool.to_owned(),
            targets,
            probe,
            interval: parse_duration(&cfg.interval),
            timeout,
            healthy_after: cfg.healthy_threshold,
            unhealthy_after: cfg.unhealthy_threshold,
            pools,
        })
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
            self.pools.observe_health(
                &self.pool,
                addr,
                result.is_ok(),
                result.as_ref().err().map(String::as_str),
                self.healthy_after,
                self.unhealthy_after,
            );
        }
    }
}

#[async_trait]
impl BackgroundService for HealthService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        self.pools.mark_checked(&self.pool);
        info!(
            pool = self.pool,
            backends = self.targets.len(),
            interval_ms = self.interval.as_millis() as u64,
            "health: checks started"
        );
        let mut rng = Rng::seeded(&self.pool);
        // First round within a second so dead backends are found quickly;
        // the random offset keeps workers from probing in lockstep.
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
    expect_status: Vec<u16>,
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
            (200..300).contains(&status)
        } else {
            self.expect_status.contains(&status)
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

/// Parse a human duration string like "10s", "500ms", "2m" into `Duration`.
/// Config validation rejects malformed values; the fallbacks here only guard
/// against unvalidated callers.
pub fn parse_duration(s: &str) -> Duration {
    crate::config::parse_duration(s).unwrap_or(Duration::from_secs(10))
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
    fn parse_durations() {
        assert_eq!(parse_duration("10s"), Duration::from_secs(10));
        assert_eq!(parse_duration("500ms"), Duration::from_millis(500));
        assert_eq!(parse_duration("2m"), Duration::from_secs(120));
        assert_eq!(parse_duration("unknown"), Duration::from_secs(10));
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
