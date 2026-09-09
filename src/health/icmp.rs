//! ICMP echo health probe.
//!
//! Host liveness only: a reply proves the host is up and reachable, nothing
//! about the service. Uses ICMP *datagram* sockets (`SOCK_DGRAM` with
//! `IPPROTO_ICMP`/`IPPROTO_ICMPV6`), which need no raw-socket privilege on
//! macOS and, on Linux, either `CAP_NET_RAW` or a group inside
//! `net.ipv4.ping_group_range`. The root master therefore opens one v4 and
//! one v6 socket per worker before dropping privileges, exactly like the
//! listeners, and the worker inherits them. Unprivileged runs try to open
//! them in-process instead.
//!
//! Every probe in the process shares the worker's two sockets. A single
//! reader task per socket matches replies to waiting probes by sequence
//! number and an 8-byte nonce; the ICMP identifier cannot be relied on
//! because Linux rewrites it per socket and macOS sockets see every reply.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tracing::{debug, error};

use super::{Probe, ProbeResult, Rng};

/// Sockets handed down by the master (see process::bind_listeners). Set
/// once at worker start; `None` entries mean "open in-process if possible".
static INHERITED: OnceLock<IcmpSockets> = OnceLock::new();

#[derive(Default)]
pub struct IcmpSockets {
    pub v4: Option<std::net::UdpSocket>,
    pub v6: Option<std::net::UdpSocket>,
}

pub fn init(sockets: IcmpSockets) {
    let _ = INHERITED.set(sockets);
}

/// Open an ICMP datagram socket for the family. Works unprivileged on macOS;
/// on Linux needs CAP_NET_RAW (root master) or `net.ipv4.ping_group_range`.
pub fn open_socket(v6: bool) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let (domain, proto) = if v6 { (Domain::IPV6, Protocol::ICMPV6) } else { (Domain::IPV4, Protocol::ICMPV4) };
    let socket = Socket::new(domain, Type::DGRAM, Some(proto))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub struct IcmpProbe {
    timeout: Duration,
}

impl IcmpProbe {
    pub fn new(timeout: Duration) -> Self {
        IcmpProbe { timeout }
    }
}

#[async_trait]
impl Probe for IcmpProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        let pinger = Pinger::global().await;
        pinger.ping(target.ip(), self.timeout).await
    }

    fn owns_timeout(&self) -> bool {
        true
    }
}

type Pending = Mutex<HashMap<u16, oneshot::Sender<()>>>;

struct Family {
    socket: Arc<UdpSocket>,
    pending: Arc<Pending>,
}

pub struct Pinger {
    v4: Option<Family>,
    v6: Option<Family>,
    seq: AtomicU16,
    nonce: [u8; 8],
}

static PINGER: tokio::sync::OnceCell<Arc<Pinger>> = tokio::sync::OnceCell::const_new();

impl Pinger {
    /// The process-wide pinger, created on first use inside a Tokio runtime
    /// (socket registration needs one).
    async fn global() -> Arc<Pinger> {
        PINGER
            .get_or_init(|| async {
                let inherited = INHERITED.get();
                let nonce = Rng::seeded("icmp").next().to_be_bytes();
                let v4 = Self::family(inherited.and_then(|s| s.v4.as_ref()), false, nonce);
                let v6 = Self::family(inherited.and_then(|s| s.v6.as_ref()), true, nonce);
                Arc::new(Pinger { v4, v6, seq: AtomicU16::new(1), nonce })
            })
            .await
            .clone()
    }

    fn family(inherited: Option<&std::net::UdpSocket>, v6: bool, nonce: [u8; 8]) -> Option<Family> {
        let std_socket = match inherited {
            Some(s) => s.try_clone(),
            None => open_socket(v6),
        };
        let std_socket = match std_socket {
            Ok(s) => s,
            Err(e) => {
                error!(
                    family = if v6 { "ipv6" } else { "ipv4" },
                    error = %e,
                    "icmp: no socket — probes of this family pass unconditionally \
                     (start as root, or allow the worker group in net.ipv4.ping_group_range)"
                );
                return None;
            }
        };
        let socket = match UdpSocket::from_std(std_socket) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!(error = %e, "icmp: cannot register socket");
                return None;
            }
        };
        let pending: Arc<Pending> = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(reader(Arc::clone(&socket), Arc::clone(&pending), v6, nonce));
        Some(Family { socket, pending })
    }

    pub async fn ping(&self, ip: IpAddr, timeout: Duration) -> ProbeResult {
        let family = match ip {
            IpAddr::V4(_) => self.v4.as_ref(),
            IpAddr::V6(_) => self.v6.as_ref(),
        };
        let Some(family) = family else {
            // Logged once at socket setup; without a socket the check cannot
            // run, and failing every backend would be worse than passing.
            return Ok(());
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        family.pending.lock().unwrap().insert(seq, tx);
        let packet = echo_request(ip.is_ipv6(), seq, &self.nonce);
        if let Err(e) = family.socket.send_to(&packet, SocketAddr::new(ip, 0)).await {
            family.pending.lock().unwrap().remove(&seq);
            return Err(super::io_reason(&e));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("reply channel closed".to_owned()),
            Err(_) => {
                family.pending.lock().unwrap().remove(&seq);
                Err(format!("no echo reply within {}ms", timeout.as_millis()))
            }
        }
    }
}

/// Deliver echo replies to the probes waiting for them.
async fn reader(socket: Arc<UdpSocket>, pending: Arc<Pending>, v6: bool, nonce: [u8; 8]) {
    let mut buf = vec![0u8; 1500];
    loop {
        let n = match socket.recv_from(&mut buf).await {
            Ok((n, _)) => n,
            Err(e) => {
                debug!(error = %e, "icmp: receive failed");
                continue;
            }
        };
        if let Some(seq) = parse_echo_reply(&buf[..n], v6, &nonce) {
            if let Some(tx) = pending.lock().unwrap().remove(&seq) {
                let _ = tx.send(());
            }
        }
    }
}

/// Echo request: type 8 (v4) or 128 (v6), code 0, checksum, id 0, seq,
/// nonce. Linux and macOS rewrite or ignore the id on datagram ICMP sockets
/// and the kernel computes the v6 checksum; the v4 checksum is filled in
/// for platforms that do not.
pub fn echo_request(v6: bool, seq: u16, nonce: &[u8; 8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.push(if v6 { 128 } else { 8 });
    p.push(0);
    p.extend_from_slice(&[0, 0]); // checksum
    p.extend_from_slice(&[0, 0]); // identifier
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(nonce);
    if !v6 {
        let sum = checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
    }
    p
}

/// Sequence number of an echo reply carrying our nonce, if `buf` is one.
/// Accepts both shapes a datagram ICMP socket delivers: bare ICMP (Linux)
/// and IP header + ICMP (macOS, IPv4 only).
pub fn parse_echo_reply(buf: &[u8], v6: bool, nonce: &[u8; 8]) -> Option<u16> {
    let icmp = if !v6 && buf.len() >= 20 && buf[0] >> 4 == 4 {
        let ihl = (buf[0] & 0x0F) as usize * 4;
        buf.get(ihl..)?
    } else {
        buf
    };
    let reply_type = if v6 { 129 } else { 0 };
    if icmp.len() < 16 || icmp[0] != reply_type || &icmp[8..16] != nonce {
        return None;
    }
    Some(u16::from_be_bytes([icmp[6], icmp[7]]))
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for chunk in data.chunks(2) {
        let word = if chunk.len() == 2 { u16::from_be_bytes([chunk[0], chunk[1]]) } else { (chunk[0] as u16) << 8 };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Whether any pool configured an `icmp` probe — the master opens sockets
/// only then.
pub fn needed(cfg: &crate::config::Config) -> bool {
    cfg.pools.values().any(|p| {
        matches!(p.health_check.as_ref().map(|h| &h.probe), Some(crate::config::ProbeConfig::Icmp))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_and_checksum() {
        let p = echo_request(false, 7, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(p[0], 8);
        assert_eq!(&p[6..8], &[0, 7]);
        assert_eq!(&p[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        // A packet with a correct checksum sums to zero.
        assert_eq!(checksum(&p), 0);
        let p6 = echo_request(true, 9, &[0; 8]);
        assert_eq!(p6[0], 128);
        assert_eq!(&p6[2..4], &[0, 0]);
    }

    #[test]
    fn parses_replies_with_and_without_ip_header() {
        let nonce = [9u8; 8];
        let mut reply = echo_request(false, 42, &nonce);
        reply[0] = 0;
        assert_eq!(parse_echo_reply(&reply, false, &nonce), Some(42));
        let mut with_ip = vec![0x45, 0, 0, 36, 0, 0, 0, 0, 64, 1, 0, 0, 127, 0, 0, 1, 127, 0, 0, 1];
        with_ip.extend_from_slice(&reply);
        assert_eq!(parse_echo_reply(&with_ip, false, &nonce), Some(42));
        // Not ours: an echo request, another process's nonce, or too short.
        assert_eq!(parse_echo_reply(&echo_request(false, 1, &nonce), false, &nonce), None);
        assert_eq!(parse_echo_reply(&reply, false, &[1; 8]), None);
        assert_eq!(parse_echo_reply(&[0; 5], false, &nonce), None);
        let mut reply6 = echo_request(true, 5, &nonce);
        reply6[0] = 129;
        assert_eq!(parse_echo_reply(&reply6, true, &nonce), Some(5));
    }

    #[tokio::test]
    async fn pings_loopback_when_a_socket_is_available() {
        // macOS allows unprivileged ICMP datagram sockets; Linux needs
        // ping_group_range. Skip rather than fail where they are refused.
        let Ok(s) = open_socket(false) else {
            eprintln!("icmp socket unavailable, skipping");
            return;
        };
        drop(s);
        let pinger = Pinger::global().await;
        assert_eq!(pinger.ping("127.0.0.1".parse().unwrap(), Duration::from_secs(2)).await, Ok(()));
        // Two concurrent pings share the socket and are demuxed by sequence.
        let (a, b) = tokio::join!(
            pinger.ping("127.0.0.1".parse().unwrap(), Duration::from_secs(2)),
            pinger.ping("127.0.0.1".parse().unwrap(), Duration::from_secs(2)),
        );
        assert_eq!((a, b), (Ok(()), Ok(())));
    }
}
