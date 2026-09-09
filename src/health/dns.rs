//! DNS health probe.
//!
//! Sends one A or AAAA query for a configured name and reads the answer. The
//! wire format needed for that is small enough to hand-roll: a fixed header,
//! one question, and answer records whose names may be compression pointers.
//! Healthy means RCODE NOERROR; `expect` additionally requires that address
//! among the answers.

use std::net::{IpAddr, SocketAddr};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use super::{io_reason, unspecified, Probe, ProbeResult, Rng};
use crate::config::{DnsRecord, DnsTransport};

pub struct DnsProbe {
    query: String,
    record: DnsRecord,
    transport: DnsTransport,
    expect: Option<IpAddr>,
}

impl DnsProbe {
    pub fn new(query: &str, record: DnsRecord, transport: DnsTransport, expect: Option<IpAddr>) -> Self {
        DnsProbe { query: query.trim_end_matches('.').to_owned(), record, transport, expect }
    }
}

#[async_trait]
impl Probe for DnsProbe {
    async fn probe(&self, target: SocketAddr) -> ProbeResult {
        let id = Rng::seeded(&self.query).next() as u16;
        let qtype = qtype_of(self.record);
        let query = build_query(id, &self.query, qtype);

        let response = match self.transport {
            DnsTransport::Udp => {
                let sock = UdpSocket::bind(unspecified(&target)).await.map_err(|e| io_reason(&e))?;
                sock.connect(target).await.map_err(|e| io_reason(&e))?;
                sock.send(&query).await.map_err(|e| io_reason(&e))?;
                let mut buf = vec![0u8; 4096];
                let n = sock.recv(&mut buf).await.map_err(|e| io_reason(&e))?;
                buf.truncate(n);
                buf
            }
            DnsTransport::Tcp => {
                let mut stream = TcpStream::connect(target).await.map_err(|e| io_reason(&e))?;
                let mut framed = (query.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(&query);
                stream.write_all(&framed).await.map_err(|e| io_reason(&e))?;
                let mut len = [0u8; 2];
                stream.read_exact(&mut len).await.map_err(|e| io_reason(&e))?;
                let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
                stream.read_exact(&mut buf).await.map_err(|e| io_reason(&e))?;
                buf
            }
        };

        let answers = parse_response(&response, id, qtype)?;
        match self.expect {
            Some(ip) if !answers.contains(&ip) => Err(format!("answer lacks {ip}")),
            _ => Ok(()),
        }
    }
}

fn qtype_of(record: DnsRecord) -> u16 {
    match record {
        DnsRecord::A => 1,
        DnsRecord::Aaaa => 28,
    }
}

/// One standard query with recursion desired, class IN.
pub fn build_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(name.len() + 18);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN, NS, AR
    for label in name.split('.').filter(|l| !l.is_empty()) {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    q
}

/// Answers of type `qtype` from a response to query `id`. `Err` names the
/// problem: bad id, not a response, non-zero RCODE, truncated packet.
pub fn parse_response(buf: &[u8], id: u16, qtype: u16) -> Result<Vec<IpAddr>, String> {
    if buf.len() < 12 {
        return Err("short response".to_owned());
    }
    if u16::from_be_bytes([buf[0], buf[1]]) != id {
        return Err("response id mismatch".to_owned());
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 == 0 {
        return Err("not a response".to_owned());
    }
    let rcode = flags & 0x000F;
    if rcode != 0 {
        return Err(format!("rcode {}", rcode_name(rcode)));
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(buf, pos)? + 4;
    }
    let mut answers = Vec::new();
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        let fixed = buf.get(pos..pos + 10).ok_or("truncated answer")?;
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let rdlen = u16::from_be_bytes([fixed[8], fixed[9]]) as usize;
        pos += 10;
        let rdata = buf.get(pos..pos + rdlen).ok_or("truncated rdata")?;
        pos += rdlen;
        if rtype != qtype {
            continue;
        }
        match (rtype, rdlen) {
            (1, 4) => answers.push(IpAddr::from([rdata[0], rdata[1], rdata[2], rdata[3]])),
            (28, 16) => {
                let mut o = [0u8; 16];
                o.copy_from_slice(rdata);
                answers.push(IpAddr::from(o));
            }
            _ => {}
        }
    }
    Ok(answers)
}

/// Position after a (possibly compressed) name starting at `pos`.
fn skip_name(buf: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let len = *buf.get(pos).ok_or("truncated name")? as usize;
        if len & 0xC0 == 0xC0 {
            return Ok(pos + 2); // pointer: two bytes, ends the name
        }
        if len == 0 {
            return Ok(pos + 1);
        }
        pos += 1 + len;
    }
}

fn rcode_name(rcode: u16) -> String {
    match rcode {
        1 => "FORMERR".to_owned(),
        2 => "SERVFAIL".to_owned(),
        3 => "NXDOMAIN".to_owned(),
        4 => "NOTIMP".to_owned(),
        5 => "REFUSED".to_owned(),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response to `query` with the given RCODE and A answers, using a
    /// compression pointer for the answer names like real servers do.
    fn response(query: &[u8], rcode: u16, a: &[[u8; 4]]) -> Vec<u8> {
        let mut r = query.to_vec();
        r[2..4].copy_from_slice(&(0x8180 | rcode).to_be_bytes());
        r[6..8].copy_from_slice(&(a.len() as u16).to_be_bytes());
        for addr in a {
            r.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
            r.extend_from_slice(&1u16.to_be_bytes()); // A
            r.extend_from_slice(&1u16.to_be_bytes()); // IN
            r.extend_from_slice(&60u32.to_be_bytes());
            r.extend_from_slice(&4u16.to_be_bytes());
            r.extend_from_slice(addr);
        }
        r
    }

    #[test]
    fn query_wire_format() {
        let q = build_query(0xBEEF, "example.com.", 1);
        assert_eq!(&q[..12], &[0xBE, 0xEF, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&q[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
    }

    #[test]
    fn parses_answers_with_pointers() {
        let q = build_query(7, "example.com", 1);
        let r = response(&q, 0, &[[93, 184, 216, 34], [10, 0, 0, 1]]);
        let answers = parse_response(&r, 7, 1).unwrap();
        assert_eq!(answers, vec!["93.184.216.34".parse::<IpAddr>().unwrap(), "10.0.0.1".parse().unwrap()]);
    }

    #[test]
    fn reports_rcode_id_and_truncation() {
        let q = build_query(7, "example.com", 1);
        assert_eq!(parse_response(&response(&q, 3, &[]), 7, 1), Err("rcode NXDOMAIN".to_owned()));
        assert_eq!(parse_response(&response(&q, 2, &[]), 7, 1), Err("rcode SERVFAIL".to_owned()));
        assert_eq!(parse_response(&response(&q, 0, &[]), 8, 1), Err("response id mismatch".to_owned()));
        assert_eq!(parse_response(&q, 7, 1), Err("not a response".to_owned()));
        let mut cut = response(&q, 0, &[[1, 1, 1, 1]]);
        cut.truncate(cut.len() - 2);
        assert_eq!(parse_response(&cut, 7, 1), Err("truncated rdata".to_owned()));
        assert_eq!(parse_response(&[0; 5], 7, 1), Err("short response".to_owned()));
    }

    #[tokio::test]
    async fn udp_probe_against_a_fake_resolver() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            loop {
                let (n, from) = server.recv_from(&mut buf).await.unwrap();
                let reply = response(&buf[..n], 0, &[[10, 0, 0, 1]]);
                server.send_to(&reply, from).await.unwrap();
            }
        });
        let ok = DnsProbe::new("example.com", DnsRecord::A, DnsTransport::Udp, None);
        assert_eq!(ok.probe(addr).await, Ok(()));
        let hit = DnsProbe::new("example.com", DnsRecord::A, DnsTransport::Udp, Some("10.0.0.1".parse().unwrap()));
        assert_eq!(hit.probe(addr).await, Ok(()));
        let miss = DnsProbe::new("example.com", DnsRecord::A, DnsTransport::Udp, Some("10.0.0.2".parse().unwrap()));
        assert_eq!(miss.probe(addr).await, Err("answer lacks 10.0.0.2".to_owned()));
    }

    #[tokio::test]
    async fn tcp_probe_against_a_fake_resolver() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut len = [0u8; 2];
                s.read_exact(&mut len).await.unwrap();
                let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
                s.read_exact(&mut q).await.unwrap();
                let reply = response(&q, 3, &[]);
                let mut framed = (reply.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(&reply);
                s.write_all(&framed).await.unwrap();
            }
        });
        let probe = DnsProbe::new("missing.example", DnsRecord::A, DnsTransport::Tcp, None);
        assert_eq!(probe.probe(addr).await, Err("rcode NXDOMAIN".to_owned()));
    }
}
