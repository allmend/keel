//! API gateway features applied per vhost or route: rate limiting, header
//! rules, and path rewriting. Each is a small, self-contained rule set
//! resolved once per request from the routing table.
//!
//! Rate limits are token buckets per (rule, client IP) inside one worker
//! process. Workers do not share buckets, so with `workers: N` a client can
//! obtain up to N times the configured rate across the process group; see
//! docs/gateway.md.

pub mod ratelimit;

use crate::config::{HeaderOps, RewriteConfig};

/// Apply `set` then `remove` to a request. Pingora keeps a case-preserving
/// name map next to the header map, so changes must go through its methods.
pub fn apply_request_ops(h: &mut pingora::http::RequestHeader, ops: &HeaderOps) {
    for (name, value) in &ops.set {
        let _ = h.insert_header(name.clone(), value.as_str());
    }
    for name in &ops.remove {
        h.remove_header(name);
    }
}

/// Same for a response.
pub fn apply_response_ops(h: &mut pingora::http::ResponseHeader, ops: &HeaderOps) {
    for (name, value) in &ops.set {
        let _ = h.insert_header(name.clone(), value.as_str());
    }
    for name in &ops.remove {
        h.remove_header(name);
    }
}

/// The upstream path for `path` under `rewrite`: `strip_prefix` removed
/// when it matches at a segment boundary, then `add_prefix` prepended.
/// `None` when nothing changes.
pub fn rewrite_path(path: &str, rewrite: &RewriteConfig) -> Option<String> {
    let mut out = path.to_owned();
    let mut changed = false;
    if let Some(prefix) = &rewrite.strip_prefix {
        let p = prefix.trim_end_matches('/');
        if !p.is_empty() && (out == p || out.starts_with(&format!("{p}/"))) {
            out = out[p.len()..].to_owned();
            if out.is_empty() {
                out = "/".to_owned();
            }
            changed = true;
        }
    }
    if let Some(prefix) = &rewrite.add_prefix {
        let p = prefix.trim_end_matches('/');
        if !p.is_empty() {
            out = format!("{p}{out}");
            changed = true;
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rw(strip: Option<&str>, add: Option<&str>) -> RewriteConfig {
        RewriteConfig { strip_prefix: strip.map(str::to_owned), add_prefix: add.map(str::to_owned) }
    }

    #[test]
    fn strip_and_add_prefixes() {
        assert_eq!(rewrite_path("/api/v1/users", &rw(Some("/api"), None)).as_deref(), Some("/v1/users"));
        assert_eq!(rewrite_path("/api", &rw(Some("/api"), None)).as_deref(), Some("/"));
        assert_eq!(rewrite_path("/api/", &rw(Some("/api/"), None)).as_deref(), Some("/"));
        assert_eq!(rewrite_path("/apiary", &rw(Some("/api"), None)), None, "segment boundary");
        assert_eq!(rewrite_path("/users", &rw(None, Some("/v1"))).as_deref(), Some("/v1/users"));
        assert_eq!(rewrite_path("/api/users", &rw(Some("/api"), Some("/internal"))).as_deref(), Some("/internal/users"));
        assert_eq!(rewrite_path("/x", &rw(None, None)), None);
    }

    #[test]
    fn header_ops_set_then_remove() {
        let ops = HeaderOps {
            set: BTreeMap::from([("X-Env".to_owned(), "prod".to_owned()), ("Server".to_owned(), "keel".to_owned())]),
            remove: vec!["x-old".to_owned(), "Server".to_owned()],
        };
        let mut req = pingora::http::RequestHeader::build("GET", b"/", None).unwrap();
        req.insert_header("x-old", "1").unwrap();
        apply_request_ops(&mut req, &ops);
        assert_eq!(req.headers.get("x-env").unwrap(), "prod");
        assert!(req.headers.get("x-old").is_none());
        assert!(req.headers.get("server").is_none(), "remove wins over set");

        let mut resp = pingora::http::ResponseHeader::build(200, None).unwrap();
        resp.insert_header("Server", "backend/1").unwrap();
        apply_response_ops(&mut resp, &ops);
        assert!(resp.headers.get("server").is_none());
        assert_eq!(resp.headers.get("x-env").unwrap(), "prod");
    }
}
