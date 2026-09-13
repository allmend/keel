//! DNS wire-format helpers for the `dns` health probe. Only what it needs:
//! names, and skipping a (possibly compressed) name. Not a resolver.

/// Encode a dotted name as labels; a trailing dot is optional. An empty
/// name encodes the root.
pub fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Position after a (possibly compressed) name starting at `pos`.
pub fn skip_name(buf: &[u8], mut pos: usize) -> Result<usize, String> {
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

pub fn rcode_name(rcode: u16) -> String {
    match rcode {
        0 => "NOERROR".to_owned(),
        1 => "FORMERR".to_owned(),
        2 => "SERVFAIL".to_owned(),
        3 => "NXDOMAIN".to_owned(),
        4 => "NOTIMP".to_owned(),
        5 => "REFUSED".to_owned(),
        6 => "YXDOMAIN".to_owned(),
        7 => "YXRRSET".to_owned(),
        8 => "NXRRSET".to_owned(),
        9 => "NOTAUTH".to_owned(),
        10 => "NOTZONE".to_owned(),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(encode_name("example.com."), b"\x07example\x03com\x00");
        assert_eq!(encode_name(""), b"\x00");
        let mut buf = encode_name("a.b");
        buf.extend_from_slice(&[0xC0, 0x00]);
        assert_eq!(skip_name(&buf, 0).unwrap(), 5);
        assert_eq!(skip_name(&buf, 5).unwrap(), 7);
        assert!(skip_name(&[3, b'a'], 0).is_err());
    }
}
