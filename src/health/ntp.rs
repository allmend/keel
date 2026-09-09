//! NTP health probe: one client-mode request (RFC 5905), healthy on a
//! server-mode reply with a non-zero stratum. Stratum 0 is a kiss-o'-death
//! packet — the server answered but refuses to serve time.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::net::UdpSocket;

use super::{io_reason, unspecified, Probe, ProbeResult};

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

pub struct NtpProbe;

#[async_trait]
impl Probe for NtpProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        let sock = UdpSocket::bind(unspecified(&target)).await.map_err(|e| io_reason(&e))?;
        sock.connect(target).await.map_err(|e| io_reason(&e))?;
        let request = build_request(now_ntp_seconds());
        sock.send(&request).await.map_err(|e| io_reason(&e))?;
        let mut buf = [0u8; 128];
        let n = sock.recv(&mut buf).await.map_err(|e| io_reason(&e))?;
        check_reply(&buf[..n], &request)
    }
}

fn now_ntp_seconds() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() + NTP_UNIX_OFFSET) as u32)
        .unwrap_or(0)
}

/// 48-byte client request: LI 0, version 4, mode 3, transmit timestamp set
/// so the reply's originate timestamp can be matched against it.
pub fn build_request(transmit_secs: u32) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = 0x23;
    p[40..44].copy_from_slice(&transmit_secs.to_be_bytes());
    p
}

pub fn check_reply(reply: &[u8], request: &[u8; 48]) -> ProbeResult {
    if reply.len() < 48 {
        return Err("short reply".to_owned());
    }
    if reply[0] & 0x07 != 4 {
        return Err(format!("mode {} reply", reply[0] & 0x07));
    }
    if reply[24..32] != request[40..48] {
        return Err("originate timestamp mismatch".to_owned());
    }
    if reply[1] == 0 {
        let code = String::from_utf8_lossy(&reply[12..16]).trim_matches(char::from(0)).to_owned();
        return Err(format!("kiss-of-death {code}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_for(request: &[u8; 48], stratum: u8, mode: u8) -> [u8; 48] {
        let mut r = [0u8; 48];
        r[0] = 0x20 | mode;
        r[1] = stratum;
        r[24..32].copy_from_slice(&request[40..48]);
        r
    }

    #[test]
    fn request_shape() {
        let r = build_request(0x1234_5678);
        assert_eq!(r[0], 0x23);
        assert_eq!(&r[40..44], &[0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn reply_checks() {
        let req = build_request(99);
        assert_eq!(check_reply(&reply_for(&req, 2, 4), &req), Ok(()));
        assert_eq!(check_reply(&reply_for(&req, 2, 3), &req), Err("mode 3 reply".to_owned()));
        let mut kod = reply_for(&req, 0, 4);
        kod[12..16].copy_from_slice(b"RATE");
        assert_eq!(check_reply(&kod, &req), Err("kiss-of-death RATE".to_owned()));
        let mut wrong = reply_for(&req, 2, 4);
        wrong[24] ^= 1;
        assert_eq!(check_reply(&wrong, &req), Err("originate timestamp mismatch".to_owned()));
        assert_eq!(check_reply(&[0; 10], &req), Err("short reply".to_owned()));
    }

    #[tokio::test]
    async fn probe_against_a_fake_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 48];
            let (_, from) = server.recv_from(&mut buf).await.unwrap();
            server.send_to(&reply_for(&buf, 3, 4), from).await.unwrap();
        });
        assert_eq!(NtpProbe.probe(addr).await, Ok(()));
    }
}
