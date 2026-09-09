//! DNS-01 record publishing through RFC 2136 dynamic updates, signed with
//! TSIG (RFC 8945). One standard protocol covers BIND, Knot, PowerDNS,
//! Windows DNS, and most enterprise resolvers, with no vendor SDK.
//!
//! The provider adds and removes `_acme-challenge.<host>` TXT records on the
//! configured primary over TCP. Responses are checked for the RCODE and the
//! ID; the response MAC is not verified — the following self-check query,
//! which must see the record before the CA is told to validate, confirms the
//! update took effect on the server that was asked.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::dns::{self, encode_name, read_records, read_u16, skip_questions, CLASS_ANY, CLASS_IN, TYPE_SOA, TYPE_TSIG, TYPE_TXT};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TsigAlgorithm {
    HmacSha256,
    HmacSha384,
    HmacSha512,
}

impl TsigAlgorithm {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "hmac-sha256" => Some(Self::HmacSha256),
            "hmac-sha384" => Some(Self::HmacSha384),
            "hmac-sha512" => Some(Self::HmacSha512),
            _ => None,
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::HmacSha256 => "hmac-sha256.",
            Self::HmacSha384 => "hmac-sha384.",
            Self::HmacSha512 => "hmac-sha512.",
        }
    }

    fn ring(self) -> ring::hmac::Algorithm {
        match self {
            Self::HmacSha256 => ring::hmac::HMAC_SHA256,
            Self::HmacSha384 => ring::hmac::HMAC_SHA384,
            Self::HmacSha512 => ring::hmac::HMAC_SHA512,
        }
    }
}

pub struct Rfc2136 {
    pub server: String,
    pub zone: String,
    pub key_name: String,
    pub key: Vec<u8>,
    pub algorithm: TsigAlgorithm,
    pub ttl: u32,
}

/// One change in the update section.
pub enum UpdateOp<'a> {
    /// Remove every TXT record at `name`.
    DeleteTxt { name: &'a str },
    /// Add one TXT record at `name`.
    AddTxt { name: &'a str, value: &'a str, ttl: u32 },
}

impl Rfc2136 {
    /// Replace the TXT records at `name` with `value`.
    pub async fn set_txt(&self, name: &str, value: &str) -> Result<()> {
        self.send(&[UpdateOp::DeleteTxt { name }, UpdateOp::AddTxt { name, value, ttl: self.ttl }]).await
    }

    pub async fn delete_txt(&self, name: &str) -> Result<()> {
        self.send(&[UpdateOp::DeleteTxt { name }]).await
    }

    /// Query the same server for the TXT records at `name`.
    pub async fn query_txt(&self, name: &str) -> Result<Vec<String>> {
        let id = crate::health::Rng::seeded(name).next() as u16;
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, QDCOUNT 1
        q.extend_from_slice(&encode_name(name));
        q.extend_from_slice(&TYPE_TXT.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        let response = exchange(&self.server, &q).await?;
        if read_u16(&response, 0).map_err(anyhow::Error::msg)? != id {
            anyhow::bail!("response id mismatch");
        }
        let rcode = read_u16(&response, 2).map_err(anyhow::Error::msg)? & 0x000F;
        if rcode != 0 {
            anyhow::bail!("query: rcode {}", dns::rcode_name(rcode));
        }
        let qd = read_u16(&response, 4).map_err(anyhow::Error::msg)? as usize;
        let an = read_u16(&response, 6).map_err(anyhow::Error::msg)? as usize;
        let pos = skip_questions(&response, 12, qd).map_err(anyhow::Error::msg)?;
        let (records, _) = read_records(&response, pos, an).map_err(anyhow::Error::msg)?;
        Ok(records.iter().filter(|r| r.rtype == TYPE_TXT).map(|r| dns::txt_value(r.rdata)).collect())
    }

    async fn send(&self, ops: &[UpdateOp<'_>]) -> Result<()> {
        let id = crate::health::Rng::seeded(&self.zone).next() as u16;
        let update = build_update(id, &self.zone, ops);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let signed = tsig_sign(&update, &self.key_name, self.algorithm, &self.key, now, 300);
        let response = exchange(&self.server, &signed).await?;
        if read_u16(&response, 0).map_err(anyhow::Error::msg)? != id {
            anyhow::bail!("update response id mismatch");
        }
        let rcode = read_u16(&response, 2).map_err(anyhow::Error::msg)? & 0x000F;
        if rcode != 0 {
            let detail = tsig_error(&response).map(|e| format!(" (TSIG: {e})")).unwrap_or_default();
            anyhow::bail!("update refused: rcode {}{detail}", dns::rcode_name(rcode));
        }
        Ok(())
    }
}

/// One TCP exchange: length-prefixed request, length-prefixed response.
async fn exchange(server: &str, msg: &[u8]) -> Result<Vec<u8>> {
    let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(server))
        .await
        .with_context(|| format!("connect {server}"))?
        .with_context(|| format!("connect {server}"))?;
    let mut framed = (msg.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(msg);
    stream.write_all(&framed).await?;
    let mut len = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut len))
        .await
        .context("dns server did not answer")??;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// UPDATE message (RFC 2136 §2): zone section names the zone, the update
/// section carries `ops`, no prerequisites.
pub fn build_update(id: u16, zone: &str, ops: &[UpdateOp<'_>]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&0x2800u16.to_be_bytes()); // opcode UPDATE
    m.extend_from_slice(&1u16.to_be_bytes()); // ZOCOUNT
    m.extend_from_slice(&0u16.to_be_bytes()); // PRCOUNT
    m.extend_from_slice(&(ops.len() as u16).to_be_bytes()); // UPCOUNT
    m.extend_from_slice(&0u16.to_be_bytes()); // ADCOUNT
    m.extend_from_slice(&encode_name(zone));
    m.extend_from_slice(&TYPE_SOA.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    for op in ops {
        match op {
            UpdateOp::DeleteTxt { name } => {
                m.extend_from_slice(&encode_name(name));
                m.extend_from_slice(&TYPE_TXT.to_be_bytes());
                m.extend_from_slice(&CLASS_ANY.to_be_bytes()); // delete RRset
                m.extend_from_slice(&0u32.to_be_bytes());
                m.extend_from_slice(&0u16.to_be_bytes());
            }
            UpdateOp::AddTxt { name, value, ttl } => {
                m.extend_from_slice(&encode_name(name));
                m.extend_from_slice(&TYPE_TXT.to_be_bytes());
                m.extend_from_slice(&CLASS_IN.to_be_bytes());
                m.extend_from_slice(&ttl.to_be_bytes());
                let rdata = txt_rdata(value);
                m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
                m.extend_from_slice(&rdata);
            }
        }
    }
    m
}

/// TXT rdata: the value as character-strings of at most 255 bytes each.
fn txt_rdata(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    for chunk in value.as_bytes().chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

/// Append a TSIG record (RFC 8945 §4.2/§4.3.3) to `msg`. The MAC covers the
/// unsigned message followed by the TSIG variables; ARCOUNT is incremented
/// in the returned message only.
pub fn tsig_sign(msg: &[u8], key_name: &str, alg: TsigAlgorithm, key: &[u8], time_signed: u64, fudge: u16) -> Vec<u8> {
    let name_wire = encode_name(&key_name.to_ascii_lowercase());
    let alg_wire = encode_name(alg.wire_name());

    // Variables in the order the RFC lists them, without MAC/original id.
    let mut vars = Vec::new();
    vars.extend_from_slice(&name_wire);
    vars.extend_from_slice(&CLASS_ANY.to_be_bytes());
    vars.extend_from_slice(&0u32.to_be_bytes()); // TTL
    vars.extend_from_slice(&alg_wire);
    vars.extend_from_slice(&time_signed.to_be_bytes()[2..]); // 48-bit
    vars.extend_from_slice(&fudge.to_be_bytes());
    vars.extend_from_slice(&0u16.to_be_bytes()); // error
    vars.extend_from_slice(&0u16.to_be_bytes()); // other len

    let mut ctx = ring::hmac::Context::with_key(&ring::hmac::Key::new(alg.ring(), key));
    ctx.update(msg);
    ctx.update(&vars);
    let mac = ctx.sign();
    let mac = mac.as_ref();

    let mut rdata = Vec::new();
    rdata.extend_from_slice(&alg_wire);
    rdata.extend_from_slice(&time_signed.to_be_bytes()[2..]);
    rdata.extend_from_slice(&fudge.to_be_bytes());
    rdata.extend_from_slice(&(mac.len() as u16).to_be_bytes());
    rdata.extend_from_slice(mac);
    rdata.extend_from_slice(&msg[0..2]); // original id
    rdata.extend_from_slice(&0u16.to_be_bytes()); // error
    rdata.extend_from_slice(&0u16.to_be_bytes()); // other len

    let mut out = msg.to_vec();
    let arcount = u16::from_be_bytes([msg[10], msg[11]]) + 1;
    out[10..12].copy_from_slice(&arcount.to_be_bytes());
    out.extend_from_slice(&name_wire);
    out.extend_from_slice(&TYPE_TSIG.to_be_bytes());
    out.extend_from_slice(&CLASS_ANY.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(&rdata);
    out
}

/// The TSIG error of a refused update, when the server included one:
/// BADSIG (wrong key), BADKEY (unknown key name), BADTIME (clock skew).
fn tsig_error(response: &[u8]) -> Option<String> {
    let qd = read_u16(response, 4).ok()? as usize;
    let an = read_u16(response, 6).ok()? as usize;
    let ns = read_u16(response, 8).ok()? as usize;
    let ar = read_u16(response, 10).ok()? as usize;
    let mut pos = skip_questions(response, 12, qd).ok()?;
    let (_, p) = read_records(response, pos, an + ns).ok()?;
    pos = p;
    let (records, _) = read_records(response, pos, ar).ok()?;
    let tsig = records.iter().find(|r| r.rtype == TYPE_TSIG)?;
    let r = tsig.rdata;
    let after_alg = crate::dns::skip_name(r, 0).ok()?;
    let mac_len = read_u16(r, after_alg + 8).ok()? as usize;
    let error = read_u16(r, after_alg + 10 + mac_len + 2).ok()?;
    Some(match error {
        0 => "ok".to_owned(),
        16 => "BADSIG — key material does not match".to_owned(),
        17 => "BADKEY — key name unknown to the server".to_owned(),
        18 => "BADTIME — clock skew beyond the fudge".to_owned(),
        n => format!("error {n}"),
    })
}

/// `_acme-challenge.<host>`; a wildcard host's challenge lives at its base.
pub fn challenge_name(host: &str) -> String {
    format!("_acme-challenge.{}", host.trim_start_matches("*."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_layout() {
        let m = build_update(7, "example.test", &[
            UpdateOp::DeleteTxt { name: "_acme-challenge.example.test" },
            UpdateOp::AddTxt { name: "_acme-challenge.example.test", value: "tok", ttl: 60 },
        ]);
        assert_eq!(&m[..12], &[0, 7, 0x28, 0, 0, 1, 0, 0, 0, 2, 0, 0]);
        // zone section
        let z = encode_name("example.test");
        assert_eq!(&m[12..12 + z.len()], &z[..]);
        let mut pos = 12 + z.len();
        assert_eq!(read_u16(&m, pos).unwrap(), TYPE_SOA);
        pos += 4;
        // delete: class ANY, ttl 0, rdlen 0
        pos = crate::dns::skip_name(&m, pos).unwrap();
        assert_eq!(read_u16(&m, pos).unwrap(), TYPE_TXT);
        assert_eq!(read_u16(&m, pos + 2).unwrap(), CLASS_ANY);
        assert_eq!(read_u16(&m, pos + 8).unwrap(), 0);
        pos += 10;
        // add: class IN, ttl 60, rdata "\x03tok"
        pos = crate::dns::skip_name(&m, pos).unwrap();
        assert_eq!(read_u16(&m, pos + 2).unwrap(), CLASS_IN);
        assert_eq!(&m[pos + 4..pos + 8], &60u32.to_be_bytes());
        assert_eq!(read_u16(&m, pos + 8).unwrap(), 4);
        assert_eq!(&m[pos + 10..pos + 14], b"\x03tok");
        assert_eq!(m.len(), pos + 14);
    }

    #[test]
    fn tsig_record_is_appended_and_counted() {
        let m = build_update(1, "example.test", &[UpdateOp::DeleteTxt { name: "x.example.test" }]);
        let key = b"0123456789abcdef0123456789abcdef";
        let signed = tsig_sign(&m, "Keel-Acme", TsigAlgorithm::HmacSha256, key, 1_700_000_000, 300);
        assert_eq!(&signed[..10], &m[..10]);
        assert_eq!(read_u16(&signed, 10).unwrap(), 1, "ARCOUNT incremented");
        assert_eq!(&signed[12..m.len()], &m[12..]);
        // The TSIG RR follows: lowercase key name, type 250, class ANY.
        let mut pos = m.len();
        assert_eq!(&signed[pos..pos + 10], b"\x09keel-acme");
        pos = crate::dns::skip_name(&signed, pos).unwrap();
        assert_eq!(read_u16(&signed, pos).unwrap(), TYPE_TSIG);
        assert_eq!(read_u16(&signed, pos + 2).unwrap(), CLASS_ANY);
        let rdlen = read_u16(&signed, pos + 8).unwrap() as usize;
        let rdata = &signed[pos + 10..pos + 10 + rdlen];
        let after_alg = crate::dns::skip_name(rdata, 0).unwrap();
        assert_eq!(&rdata[..after_alg], &encode_name("hmac-sha256.")[..]);
        assert_eq!(&rdata[after_alg..after_alg + 6], &1_700_000_000u64.to_be_bytes()[2..]);
        assert_eq!(read_u16(rdata, after_alg + 6).unwrap(), 300);
        let mac_len = read_u16(rdata, after_alg + 8).unwrap() as usize;
        assert_eq!(mac_len, 32);
        assert_eq!(read_u16(rdata, after_alg + 10 + mac_len).unwrap(), 1, "original id");
        // The same inputs sign identically; a different key does not.
        assert_eq!(signed, tsig_sign(&m, "keel-acme", TsigAlgorithm::HmacSha256, key, 1_700_000_000, 300));
        assert_ne!(signed, tsig_sign(&m, "keel-acme", TsigAlgorithm::HmacSha256, b"other-key", 1_700_000_000, 300));
    }

    #[test]
    fn challenge_names() {
        assert_eq!(challenge_name("example.com"), "_acme-challenge.example.com");
        assert_eq!(challenge_name("*.example.com"), "_acme-challenge.example.com");
        assert_eq!(TsigAlgorithm::parse("HMAC-SHA512"), Some(TsigAlgorithm::HmacSha512));
        assert_eq!(TsigAlgorithm::parse("hmac-md5"), None);
    }
}
