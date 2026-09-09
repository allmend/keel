//! Inbound PROXY protocol (HAProxy spec v1 and v2).
//!
//! A load balancer in front of Keel that terminates the client's TCP
//! connection (AWS NLB, HAProxy, another Keel) can prepend a header carrying
//! the original client address. Keel reads it once per connection — or, for
//! UDP, once per datagram — before anything else touches the bytes, and
//! treats the address it names as the client for routing keys, forwarded
//! headers, access logs, and passive health.
//!
//! The spec is explicit that a listener expecting the header must reject a
//! connection that does not start with one; there is no fallback, because a
//! fallback would let any client claim any address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use pingora::protocols::Stream;
use tokio::io::AsyncReadExt;

/// Addresses carried by a PROXY header. `None` for `LOCAL` (v2) and
/// `UNKNOWN` (v1): a health check from the load balancer itself, to be
/// handled as coming from the socket's own peer.
pub type Addresses = Option<(SocketAddr, SocketAddr)>;

#[derive(Debug, PartialEq)]
pub struct Header {
    pub consumed: usize,
    pub addresses: Addresses,
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Not a PROXY header at all, or malformed.
    Invalid(&'static str),
    /// A valid prefix; more bytes are needed to finish.
    Incomplete,
}

const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\x00\r\nQUIT\n";
const V1_MAX: usize = 107;

/// Parse a header at the start of `buf`.
pub fn parse(buf: &[u8]) -> Result<Header, Error> {
    if buf.starts_with(b"PROXY ") || (buf.len() < 6 && b"PROXY ".starts_with(buf)) {
        return parse_v1(buf);
    }
    if buf.starts_with(V2_SIGNATURE) || (buf.len() < 12 && V2_SIGNATURE.starts_with(buf)) {
        return parse_v2(buf);
    }
    Err(Error::Invalid("no PROXY header"))
}

fn parse_v1(buf: &[u8]) -> Result<Header, Error> {
    let Some(end) = buf.windows(2).position(|w| w == b"\r\n") else {
        return if buf.len() > V1_MAX { Err(Error::Invalid("v1 line too long")) } else { Err(Error::Incomplete) };
    };
    if end + 2 > V1_MAX {
        return Err(Error::Invalid("v1 line too long"));
    }
    let line = std::str::from_utf8(&buf[..end]).map_err(|_| Error::Invalid("v1 line not ascii"))?;
    let mut parts = line.split(' ');
    if parts.next() != Some("PROXY") {
        return Err(Error::Invalid("v1 prefix"));
    }
    let addresses = match parts.next() {
        Some("UNKNOWN") => None,
        Some(family @ ("TCP4" | "TCP6")) => {
            let src: IpAddr = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::Invalid("v1 source address"))?;
            let dst: IpAddr = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::Invalid("v1 destination address"))?;
            let sport: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::Invalid("v1 source port"))?;
            let dport: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or(Error::Invalid("v1 destination port"))?;
            if parts.next().is_some() {
                return Err(Error::Invalid("v1 trailing fields"));
            }
            let v4 = family == "TCP4";
            if src.is_ipv4() != v4 || dst.is_ipv4() != v4 {
                return Err(Error::Invalid("v1 address family mismatch"));
            }
            Some((SocketAddr::new(src, sport), SocketAddr::new(dst, dport)))
        }
        _ => return Err(Error::Invalid("v1 family")),
    };
    Ok(Header { consumed: end + 2, addresses })
}

fn parse_v2(buf: &[u8]) -> Result<Header, Error> {
    if buf.len() < 16 {
        return Err(Error::Incomplete);
    }
    if buf[12] >> 4 != 2 {
        return Err(Error::Invalid("v2 version"));
    }
    let command = buf[12] & 0x0F;
    let family = buf[13] >> 4;
    let len = u16::from_be_bytes([buf[14], buf[15]]) as usize;
    let total = 16 + len;
    if buf.len() < total {
        return Err(Error::Incomplete);
    }
    let body = &buf[16..total];
    let addresses = match command {
        0 => None, // LOCAL
        1 => match family {
            1 => {
                if body.len() < 12 {
                    return Err(Error::Invalid("v2 short IPv4 block"));
                }
                let src = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
                let dst = Ipv4Addr::new(body[4], body[5], body[6], body[7]);
                let sport = u16::from_be_bytes([body[8], body[9]]);
                let dport = u16::from_be_bytes([body[10], body[11]]);
                Some((SocketAddr::new(src.into(), sport), SocketAddr::new(dst.into(), dport)))
            }
            2 => {
                if body.len() < 36 {
                    return Err(Error::Invalid("v2 short IPv6 block"));
                }
                let mut s = [0u8; 16];
                let mut d = [0u8; 16];
                s.copy_from_slice(&body[0..16]);
                d.copy_from_slice(&body[16..32]);
                let sport = u16::from_be_bytes([body[32], body[33]]);
                let dport = u16::from_be_bytes([body[34], body[35]]);
                Some((SocketAddr::new(Ipv6Addr::from(s).into(), sport), SocketAddr::new(Ipv6Addr::from(d).into(), dport)))
            }
            0 => None, // AF_UNSPEC with PROXY command: sender knows nothing
            _ => return Err(Error::Invalid("v2 address family")),
        },
        _ => return Err(Error::Invalid("v2 command")),
    };
    Ok(Header { consumed: total, addresses })
}

/// Read and consume the header at the start of a Pingora stream. Pingora's
/// peek reads exactly the requested length, so the read is staged: six bytes
/// to tell the versions apart, then the v1 line up to CRLF or the v2 body by
/// its length field.
pub async fn read_from_stream(stream: &mut Stream) -> Result<Addresses, Error> {
    let mut prefix = [0u8; 6];
    stream.try_peek(&mut prefix).await.map_err(|_| Error::Invalid("connection closed before header"))?;
    if &prefix == b"PROXY " {
        // v1: consume byte by byte up to CRLF; the line is at most 107 bytes.
        let mut line = Vec::with_capacity(64);
        loop {
            let b = stream.read_u8().await.map_err(|_| Error::Invalid("connection closed inside v1 header"))?;
            line.push(b);
            if line.ends_with(b"\r\n") {
                break;
            }
            if line.len() > V1_MAX {
                return Err(Error::Invalid("v1 line too long"));
            }
        }
        return parse_v1(&line).map(|h| h.addresses);
    }
    let mut fixed = [0u8; 16];
    stream.try_peek(&mut fixed).await.map_err(|_| Error::Invalid("connection closed before header"))?;
    if !fixed.starts_with(V2_SIGNATURE) {
        return Err(Error::Invalid("no PROXY header"));
    }
    let len = u16::from_be_bytes([fixed[14], fixed[15]]) as usize;
    let mut whole = vec![0u8; 16 + len];
    stream.read_exact(&mut whole).await.map_err(|_| Error::Invalid("connection closed inside v2 header"))?;
    parse_v2(&whole).map(|h| h.addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_examples_from_the_spec() {
        let h = parse(b"PROXY TCP4 255.255.255.255 255.255.255.255 65535 65535\r\nGET /").unwrap();
        assert_eq!(h.consumed, 56);
        assert_eq!(h.addresses, Some(("255.255.255.255:65535".parse().unwrap(), "255.255.255.255:65535".parse().unwrap())));
        let h = parse(b"PROXY TCP6 ffff:f...f:ffff ffff:f...f:ffff 65535 65535\r\n");
        assert!(matches!(h, Err(Error::Invalid(_))), "abbreviated spec placeholder is not a real address");
        let h = parse(b"PROXY TCP6 2001:db8::1 2001:db8::2 12345 443\r\n").unwrap();
        assert_eq!(h.addresses.unwrap().0, "[2001:db8::1]:12345".parse().unwrap());
        assert_eq!(parse(b"PROXY UNKNOWN\r\n").unwrap(), Header { consumed: 15, addresses: None });
        assert_eq!(parse(b"PROXY TCP4 1.2.3.4"), Err(Error::Incomplete));
        assert_eq!(parse(b"PRO"), Err(Error::Incomplete));
        assert_eq!(parse(b"PROXY TCP4 ::1 ::2 1 2\r\n"), Err(Error::Invalid("v1 address family mismatch")));
        assert_eq!(parse(b"GET / HTTP/1.1\r\n"), Err(Error::Invalid("no PROXY header")));
    }

    fn v2(command: u8, family: u8, body: &[u8]) -> Vec<u8> {
        let mut h = V2_SIGNATURE.to_vec();
        h.push(0x20 | command);
        h.push(family << 4 | 1);
        h.extend_from_slice(&(body.len() as u16).to_be_bytes());
        h.extend_from_slice(body);
        h
    }

    #[test]
    fn v2_binary_headers() {
        let mut body = vec![203, 0, 113, 42, 10, 0, 0, 1];
        body.extend_from_slice(&12345u16.to_be_bytes());
        body.extend_from_slice(&80u16.to_be_bytes());
        let mut msg = v2(1, 1, &body);
        msg.extend_from_slice(b"GET / HTTP/1.1\r\n");
        let h = parse(&msg).unwrap();
        assert_eq!(h.consumed, 28);
        assert_eq!(h.addresses, Some(("203.0.113.42:12345".parse().unwrap(), "10.0.0.1:80".parse().unwrap())));

        let mut body6 = vec![0u8; 36];
        body6[15] = 1; // ::1
        body6[31] = 2; // ::2
        body6[33] = 7;
        body6[35] = 9;
        let h = parse(&v2(1, 2, &body6)).unwrap();
        assert_eq!(h.addresses, Some(("[::1]:7".parse().unwrap(), "[::2]:9".parse().unwrap())));

        // LOCAL: no addresses, body (TLVs) skipped by length.
        assert_eq!(parse(&v2(0, 0, &[1, 2, 3])).unwrap(), Header { consumed: 19, addresses: None });
        // Truncated body, wrong version, bad command.
        assert_eq!(parse(&v2(1, 1, &body)[..20]), Err(Error::Incomplete));
        let mut bad = v2(1, 1, &body);
        bad[12] = 0x11;
        assert_eq!(parse(&bad), Err(Error::Invalid("v2 version")));
        assert_eq!(parse(&v2(5, 1, &body)), Err(Error::Invalid("v2 command")));
        assert_eq!(parse(&V2_SIGNATURE[..5]), Err(Error::Incomplete));
    }
}
