use crate::config::{Config, ForwardedHeadersConfig};
use std::collections::HashMap;

/// Maps an incoming (host, path) pair to a pool name.
///
/// Resolution order:
///   1. Exact host match, longest path-prefix match
///   2. Wildcard host ("*"), longest path-prefix match
pub struct RoutingTable {
    /// host → routes sorted by path prefix length descending
    vhosts: HashMap<String, Vec<Route>>,
    /// host → forwarded headers config (cloned from config on build)
    forwarded: HashMap<String, ForwardedHeadersConfig>,
}

struct Route {
    path_prefix: String,
    pool: String,
}

impl RoutingTable {
    pub fn build(cfg: &Config) -> Self {
        let mut vhosts: HashMap<String, Vec<Route>> = HashMap::new();
        let mut forwarded: HashMap<String, ForwardedHeadersConfig> = HashMap::new();

        for vhost in &cfg.vhosts {
            let routes = vhosts.entry(vhost.host.clone()).or_default();

            if vhost.routes.is_empty() {
                // Single pool for the whole vhost — treat as "/"
                if let Some(pool) = &vhost.pool {
                    routes.push(Route {
                        path_prefix: "/".into(),
                        pool: pool.clone(),
                    });
                }
            } else {
                for r in &vhost.routes {
                    routes.push(Route {
                        path_prefix: r.path.clone(),
                        pool: r.pool.clone(),
                    });
                }
            }

            // Longest prefix first
            routes.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));

            if let Some(fwd) = &vhost.forwarded_headers {
                forwarded.insert(vhost.host.clone(), fwd.clone());
            }
        }

        RoutingTable { vhosts, forwarded }
    }

    /// Returns the pool name for the given host and path, or `None` if no match.
    pub fn resolve<'a>(&'a self, host: &str, path: &str) -> Option<&'a str> {
        // Strip port from Host header if present ("example.com:443" → "example.com")
        let host = host.split(':').next().unwrap_or(host);

        self.match_host(host, path)
            .or_else(|| self.match_host("*", path))
    }

    /// Returns the forwarded headers config for the given host, or `None` if not set.
    /// Falls back to the wildcard vhost config if no exact match.
    pub fn forwarded_config(&self, host: &str) -> Option<&ForwardedHeadersConfig> {
        let host = host.split(':').next().unwrap_or(host);
        self.forwarded.get(host).or_else(|| self.forwarded.get("*"))
    }

    fn match_host<'a>(&'a self, host: &str, path: &str) -> Option<&'a str> {
        let routes = self.vhosts.get(host)?;
        routes
            .iter()
            .find(|r| path.starts_with(r.path_prefix.as_str()))
            .map(|r| r.pool.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessLogConfig, Config, KeelConfig, MetricsConfig, Pool, LbAlgorithm, Backend, Vhost, Route as CfgRoute};
    use std::collections::HashMap;

    fn make_config(vhosts: Vec<Vhost>, pools: HashMap<String, Pool>) -> Config {
        Config {
            path: String::new(),
            conf_dir: None,
            keel: KeelConfig::default(),
            listeners: vec![],
            metrics: MetricsConfig::default(),
            pools,
            vhosts,
            access_log: AccessLogConfig::default(),
            include: vec![],
            cluster: None,
        }
    }

    fn pool(name: &str) -> (String, Pool) {
        (name.into(), Pool {
            algorithm: LbAlgorithm::RoundRobin,
            health_check: None,
            backends: vec![Backend { address: "127.0.0.1:8080".into(), weight: 1 }],
        })
    }

    fn vhost(host: &str, pool: &str) -> Vhost {
        Vhost {
            host: host.into(),
            pool: Some(pool.into()),
            routes: vec![],
            tls: None,
            forwarded_headers: None,
        }
    }

    #[test]
    fn exact_host_match() {
        let cfg = make_config(vec![vhost("api.example.com", "api")], [pool("api")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(t.resolve("api.example.com", "/foo"), Some("api"));
        assert_eq!(t.resolve("other.com", "/foo"), None);
    }

    #[test]
    fn path_prefix_longest_wins() {
        let cfg = make_config(
            vec![Vhost {
                host: "example.com".into(),
                pool: None,
                routes: vec![
                    CfgRoute { path: "/".into(), pool: "default".into() },
                    CfgRoute { path: "/api/".into(), pool: "api".into() },
                ],
                tls: None,
                forwarded_headers: None,
            }],
            [pool("default"), pool("api")].into(),
        );
        let t = RoutingTable::build(&cfg);
        assert_eq!(t.resolve("example.com", "/api/users"), Some("api"));
        assert_eq!(t.resolve("example.com", "/static/x"), Some("default"));
    }

    #[test]
    fn wildcard_fallback() {
        let cfg = make_config(vec![vhost("*", "default")], [pool("default")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(t.resolve("anything.com", "/"), Some("default"));
    }

    #[test]
    fn host_with_port_stripped() {
        let cfg = make_config(vec![vhost("example.com", "web")], [pool("web")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(t.resolve("example.com:8080", "/"), Some("web"));
    }
}
