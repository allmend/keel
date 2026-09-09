//! DNS wire-format helpers shared by the `dns` health probe and the RFC 2136
//! ACME provider. Only what those need: names, the fixed header, and record
//! walking. Not a resolver.

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

pub fn read_u16(buf: &[u8], pos: usize) -> Result<u16, String> {
    let b = buf.get(pos..pos + 2).ok_or("truncated")?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
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

/// One resource record from the answer, authority, or additional section.
pub struct Record<'a> {
    pub rtype: u16,
    pub rdata: &'a [u8],
}

/// Walk a response: skip `qdcount` questions, then yield `count` records
/// starting at `pos`. Returns the records and the position after them.
pub fn read_records(buf: &[u8], mut pos: usize, count: usize) -> Result<(Vec<Record<'_>>, usize), String> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        pos = skip_name(buf, pos)?;
        let fixed = buf.get(pos..pos + 10).ok_or("truncated record")?;
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let rdlen = u16::from_be_bytes([fixed[8], fixed[9]]) as usize;
        pos += 10;
        let rdata = buf.get(pos..pos + rdlen).ok_or("truncated rdata")?;
        pos += rdlen;
        out.push(Record { rtype, rdata });
    }
    Ok((out, pos))
}

/// Skip `count` question entries starting at `pos`.
pub fn skip_questions(buf: &[u8], mut pos: usize, count: usize) -> Result<usize, String> {
    for _ in 0..count {
        pos = skip_name(buf, pos)? + 4;
    }
    Ok(pos)
}

/// The strings of a TXT rdata (RFC 1035 character-strings), concatenated
/// the way clients compare them.
pub fn txt_value(rdata: &[u8]) -> String {
    let mut out = String::new();
    let mut pos = 0;
    while pos < rdata.len() {
        let len = rdata[pos] as usize;
        pos += 1;
        let end = (pos + len).min(rdata.len());
        out.push_str(&String::from_utf8_lossy(&rdata[pos..end]));
        pos = end;
    }
    out
}

pub const TYPE_SOA: u16 = 6;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_TSIG: u16 = 250;
pub const CLASS_IN: u16 = 1;
pub const CLASS_ANY: u16 = 255;

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

    #[test]
    fn txt_strings_concatenate() {
        assert_eq!(txt_value(b"\x03abc\x02de"), "abcde");
        assert_eq!(txt_value(b""), "");
        assert_eq!(txt_value(b"\x05ab"), "ab"); // short final string is tolerated
    }
}
