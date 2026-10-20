//! Client access lists: `allow` and `deny` networks on a vhost, a route, or
//! a `tcp` or `udp` listener.

use std::net::IpAddr;

use ipnet::IpNet;

/// Deny first; then, when `allow` lists anything, only what it lists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccessList {
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
}

impl AccessList {
    /// `None` when both lists are empty: everyone is let in.
    pub fn new(allow: &[String], deny: &[String]) -> Result<Option<Self>, String> {
        if allow.is_empty() && deny.is_empty() {
            return Ok(None);
        }
        let parse = |nets: &[String]| nets.iter().map(|n| parse_net(n)).collect::<Result<Vec<_>, _>>();
        Ok(Some(AccessList { allow: parse(allow)?, deny: parse(deny)? }))
    }

    pub fn permits(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        !self.deny.iter().any(|n| n.contains(&ip)) && (self.allow.is_empty() || self.allow.iter().any(|n| n.contains(&ip)))
    }
}

/// A network, or one address.
pub fn parse_net(text: &str) -> Result<IpNet, String> {
    text.parse::<IpNet>()
        .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
        .map_err(|_| format!("'{text}' is not an IP network or address"))
}

/// The client behind `peer`: when `peer` is a trusted proxy, the last
/// `X-Forwarded-For` entry that is not one.
pub fn client_ip(peer: IpAddr, forwarded_for: Option<&str>, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|n| n.contains(&ip.to_canonical()));
    if !is_trusted(&peer) {
        return peer;
    }
    let mut client = peer;
    for hop in forwarded_for.unwrap_or("").rsplit(',') {
        match hop.trim().parse::<IpAddr>() {
            Ok(ip) => {
                client = ip;
                if !is_trusted(&ip) {
                    break;
                }
            }
            // An entry that is no address ends what can be trusted.
            Err(_) => break,
        }
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(allow: &[&str], deny: &[&str]) -> AccessList {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        AccessList::new(&own(allow), &own(deny)).unwrap().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn deny_first_then_allow() {
        let l = list(&["10.0.0.0/8", "2001:db8::/32"], &["10.0.0.5"]);
        assert!(l.permits(ip("10.1.2.3")));
        assert!(!l.permits(ip("10.0.0.5")));
        assert!(!l.permits(ip("192.0.2.1")));
        assert!(l.permits(ip("2001:db8::1")));
        assert!(l.permits(ip("::ffff:10.1.2.3")), "a v4-mapped address is its v4 address");
        let only_deny = list(&[], &["192.0.2.0/24"]);
        assert!(!only_deny.permits(ip("192.0.2.9")));
        assert!(only_deny.permits(ip("198.51.100.1")));
        assert_eq!(AccessList::new(&[], &[]), Ok(None));
        assert_eq!(AccessList::new(&["nope".into()], &[]), Err("'nope' is not an IP network or address".into()));
    }

    #[test]
    fn the_client_is_the_last_hop_no_trusted_proxy_added() {
        let trusted = vec![parse_net("10.0.0.0/8").unwrap()];
        assert_eq!(client_ip(ip("192.0.2.1"), Some("198.51.100.7"), &trusted), ip("192.0.2.1"), "untrusted peer: its header is ignored");
        assert_eq!(client_ip(ip("10.0.0.1"), Some("6.6.6.6, 198.51.100.7, 10.0.0.2"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(ip("10.0.0.1"), None, &trusted), ip("10.0.0.1"));
        assert_eq!(client_ip(ip("10.0.0.1"), Some("junk, 10.0.0.3"), &trusted), ip("10.0.0.3"));
    }
}
