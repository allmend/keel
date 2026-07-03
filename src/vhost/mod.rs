use crate::config::{Config, ForwardedHeadersConfig, VhostCacheConfig};
use std::collections::{HashMap, HashSet};

/// Maps an incoming (host, path) pair to a pool name.
///
/// Resolution order:
///  1. Exact host match, longest path-prefix match
///  2. Wildcard host ("*"), longest path-prefix match
pub struct RoutingTable {
    /// host → routes sorted by path prefix length descending
    vhosts: HashMap<String, Vec<Route>>,
    /// host → forwarded headers config (cloned from config on build)
    forwarded: HashMap<String, ForwardedHeadersConfig>,
    /// host → vhost-level cache config (fallback when route has none)
    cache: HashMap<String, VhostCacheConfig>,
    /// hosts that should redirect HTTP → HTTPS
    redirect_http: HashSet<String>,
}

struct Route {
    path_prefix: String,
    pool: String,
    /// Route-level cache config — overrides the vhost-level config when present.
    cache: Option<VhostCacheConfig>,
}

impl RoutingTable {
    pub fn build(cfg: &Config) -> Self {
        let mut vhosts: HashMap<String, Vec<Route>> = HashMap::new();
        let mut forwarded: HashMap<String, ForwardedHeadersConfig> = HashMap::new();
        let mut cache: HashMap<String, VhostCacheConfig> = HashMap::new();
        let mut redirect_http: HashSet<String> = HashSet::new();

        for vhost in &cfg.vhosts {
            let routes = vhosts.entry(vhost.host.clone()).or_default();

            if vhost.routes.is_empty() {
                // Single pool for the whole vhost — treat as "/"
                if let Some(pool) = &vhost.pool {
                    routes.push(Route {
                        path_prefix: "/".into(),
                        pool: pool.clone(),
                        cache: None,
                    });
                }
            } else {
                for r in &vhost.routes {
                    routes.push(Route {
                        path_prefix: r.path.clone(),
                        pool: r.pool.clone(),
                        cache: r.cache.clone(),
                    });
                }
            }

            // Longest prefix first
            routes.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));

            if let Some(fwd) = &vhost.forwarded_headers {
                forwarded.insert(vhost.host.clone(), fwd.clone());
            }

            if let Some(cc) = &vhost.cache {
                cache.insert(vhost.host.clone(), cc.clone());
            }

            if vhost.redirect_http {
                redirect_http.insert(vhost.host.clone());
            }
        }

        RoutingTable { vhosts, forwarded, cache, redirect_http }
    }

    /// Returns the pool name for the given host and path, or `None` if no match.
    pub fn resolve<'a>(&'a self, host: &str, path: &str) -> Option<&'a str> {
        let host = host.split(':').next().unwrap_or(host);
        self.match_host(host, path)
            .or_else(|| self.match_host("*", path))
    }

    /// Returns the cache config for the given host and path, or `None` if caching is
    /// not enabled. Resolution order: route-level → vhost-level → wildcard vhost-level.
    pub fn cache_config(&self, host: &str, path: &str) -> Option<&VhostCacheConfig> {
        let host = host.split(':').next().unwrap_or(host);

        let route_cache = self.route_cache(host, path)
            .or_else(|| self.route_cache("*", path));

        if let Some(cc) = route_cache {
            return if cc.enabled { Some(cc) } else { None };
        }

        // Fall back to vhost-level config.
        let vhost_cache = self.cache.get(host).or_else(|| self.cache.get("*"))?;
        if vhost_cache.enabled { Some(vhost_cache) } else { None }
    }

    /// Maps an incoming Host header to the canonical configured vhost label that
    /// serves it: the exact host key if configured, else `"*"` if a wildcard vhost
    /// exists, else `None`. Used as a *bounded* key for access logs and metrics so
    /// an attacker-supplied Host header cannot create unbounded files or metric
    /// series. The returned value is always one of the operator-configured hosts.
    pub fn vhost_label(&self, host: &str) -> Option<&str> {
        let host = host.split(':').next().unwrap_or(host);
        if let Some((key, _)) = self.vhosts.get_key_value(host) {
            Some(key.as_str())
        } else if self.vhosts.contains_key("*") {
            Some("*")
        } else {
            None
        }
    }

    /// Returns the forwarded headers config for the given host, or `None` if not set.
    pub fn forwarded_config(&self, host: &str) -> Option<&ForwardedHeadersConfig> {
        let host = host.split(':').next().unwrap_or(host);
        self.forwarded.get(host).or_else(|| self.forwarded.get("*"))
    }

    /// Returns true if plain HTTP requests to this host should be redirected to HTTPS.
    pub fn should_redirect_https(&self, host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host);
        self.redirect_http.contains(host) || self.redirect_http.contains("*")
    }

    fn match_host<'a>(&'a self, host: &str, path: &str) -> Option<&'a str> {
        let routes = self.vhosts.get(host)?;
        routes
            .iter()
            .find(|r| path.starts_with(r.path_prefix.as_str()))
            .map(|r| r.pool.as_str())
    }

    /// Returns the route-level cache config for the matching route, if any.
    fn route_cache<'a>(&'a self, host: &str, path: &str) -> Option<&'a VhostCacheConfig> {
        let routes = self.vhosts.get(host)?;
        routes
            .iter()
            .find(|r| path.starts_with(r.path_prefix.as_str()))
            .and_then(|r| r.cache.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccessLogConfig, Backend, Config, KeelConfig, LbAlgorithm, MetricsConfig, Pool,
        Route as CfgRoute, Vhost, VhostCacheConfig,
    };
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
            cache: crate::config::CacheConfig::default(),
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
            cache: None,
            redirect_http: false,
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
                    CfgRoute { path: "/".into(), pool: "default".into(), cache: None },
                    CfgRoute { path: "/api/".into(), pool: "api".into(), cache: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: None,
                redirect_http: false,
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

    #[test]
    fn cache_config_route_overrides_vhost() {
        let cfg = make_config(
            vec![Vhost {
                host: "example.com".into(),
                pool: None,
                routes: vec![
                    CfgRoute {
                        path: "/static/".into(),
                        pool: "assets".into(),
                        cache: Some(VhostCacheConfig {
                            enabled: true,
                            ttl: Some(3600),
                            statuses: vec![200],
                            content_types: vec!["image/*".into()],
                        }),
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig {
                    enabled: true,
                    ttl: Some(60),
                    statuses: vec![],
                    content_types: vec![],
                }),
                redirect_http: false,
            }],
            [pool("assets"), pool("web")].into(),
        );
        let t = RoutingTable::build(&cfg);

        // /static/ uses route-level config (ttl 3600, image/*)
        let cc = t.cache_config("example.com", "/static/logo.png").unwrap();
        assert_eq!(cc.ttl, Some(3600));
        assert_eq!(cc.content_types, vec!["image/*"]);

        // / falls back to vhost-level config (ttl 60, no content_types filter)
        let cc = t.cache_config("example.com", "/").unwrap();
        assert_eq!(cc.ttl, Some(60));
        assert!(cc.content_types.is_empty());
    }

    #[test]
    fn cache_config_disabled_route_hides_vhost_default() {
        let cfg = make_config(
            vec![Vhost {
                host: "example.com".into(),
                pool: None,
                routes: vec![
                    CfgRoute {
                        path: "/api/".into(),
                        pool: "api".into(),
                        cache: Some(VhostCacheConfig { enabled: false, ..Default::default() }),
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig { enabled: true, ttl: Some(60), ..Default::default() }),
                redirect_http: false,
            }],
            [pool("api"), pool("web")].into(),
        );
        let t = RoutingTable::build(&cfg);

        // /api/ explicitly disabled — must not fall through to vhost default
        assert!(t.cache_config("example.com", "/api/users").is_none());

        // / still uses vhost default
        assert!(t.cache_config("example.com", "/").is_some());
    }
}
