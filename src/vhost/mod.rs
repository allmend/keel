use crate::config::{
    Config, DefaultAction, ForwardedHeadersConfig, HeaderRules, RateLimitConfig, RewriteConfig,
    VhostCacheConfig,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Gateway rules in effect for one matched route: the route's own value for
/// each field, else the vhost's.
#[derive(Debug, Default)]
pub struct GatewayRules {
    /// Bucket key prefix for the rate limiter: host plus path prefix.
    pub id: String,
    pub rate_limit: Option<RateLimitConfig>,
    pub headers: Option<HeaderRules>,
    pub rewrite: Option<RewriteConfig>,
}

impl GatewayRules {
    pub fn is_empty(&self) -> bool {
        self.rate_limit.is_none() && self.headers.is_none() && self.rewrite.is_none()
    }
}

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
    /// host → direct response without a pool (redirect or static status)
    default_actions: HashMap<String, DefaultAction>,
}

struct Route {
    path_prefix: String,
    pool: String,
    /// Route-level cache config — overrides the vhost-level config when present.
    cache: Option<VhostCacheConfig>,
    rules: Arc<GatewayRules>,
}

impl RoutingTable {
    pub fn build(cfg: &Config) -> Self {
        let mut vhosts: HashMap<String, Vec<Route>> = HashMap::new();
        let mut forwarded: HashMap<String, ForwardedHeadersConfig> = HashMap::new();
        let mut cache: HashMap<String, VhostCacheConfig> = HashMap::new();
        let mut redirect_http: HashSet<String> = HashSet::new();
        let mut default_actions: HashMap<String, DefaultAction> = HashMap::new();

        for vhost in &cfg.vhosts {
            let routes = vhosts.entry(vhost.host.clone()).or_default();

            if vhost.routes.is_empty() {
                // Single pool for the whole vhost — treat as "/"
                if let Some(pool) = &vhost.pool {
                    routes.push(Route {
                        path_prefix: "/".into(),
                        pool: pool.clone(),
                        cache: None,
                        rules: Arc::new(GatewayRules {
                            id: format!("{}/", vhost.host),
                            rate_limit: vhost.rate_limit.clone(),
                            headers: vhost.headers.clone(),
                            rewrite: vhost.rewrite.clone(),
                        }),
                    });
                }
            } else {
                for r in &vhost.routes {
                    routes.push(Route {
                        path_prefix: r.path.clone(),
                        pool: r.pool.clone(),
                        cache: r.cache.clone(),
                        rules: Arc::new(GatewayRules {
                            id: format!("{}{}", vhost.host, r.path),
                            rate_limit: r.rate_limit.clone().or_else(|| vhost.rate_limit.clone()),
                            headers: r.headers.clone().or_else(|| vhost.headers.clone()),
                            rewrite: r.rewrite.clone().or_else(|| vhost.rewrite.clone()),
                        }),
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

            if vhost.redirect_http_effective() {
                redirect_http.insert(vhost.host.clone());
            }

            if let Some(action) = &vhost.default_action {
                default_actions.insert(vhost.host.clone(), action.clone());
            }
        }

        RoutingTable { vhosts, forwarded, cache, redirect_http, default_actions }
    }

    /// Returns the default action for the given host: exact match first, then
    /// the wildcard vhost. An exact vhost with a pool shadows the wildcard's
    /// default action (the request routes normally).
    pub fn default_action(&self, host: &str) -> Option<&DefaultAction> {
        let host = host.split(':').next().unwrap_or(host);
        if let Some(a) = self.default_actions.get(host) {
            return Some(a);
        }
        // Wildcard default action applies only when no exact vhost matched.
        if !self.vhosts.contains_key(host) || self.vhosts.get(host).is_some_and(|r| r.is_empty()) {
            return self.default_actions.get("*");
        }
        None
    }

    /// Returns the pool name for the given host and path, or `None` if no match.
    /// Order: exact host, then its one-label wildcard (`*.example.com` for
    /// `api.example.com`), then the catch-all `"*"`.
    pub fn resolve<'a>(&'a self, host: &str, path: &str) -> Option<&'a str> {
        let host = host.split(':').next().unwrap_or(host);
        self.match_host(host, path)
            .or_else(|| wildcard_of(host).and_then(|w| self.match_host(&w, path)))
            .or_else(|| self.match_host("*", path))
    }

    /// Returns the cache config for the given host and path, or `None` if caching is
    /// not enabled. Resolution order: route-level → vhost-level → wildcard vhost-level.
    pub fn cache_config(&self, host: &str, path: &str) -> Option<&VhostCacheConfig> {
        let host = host.split(':').next().unwrap_or(host);

        let wildcard = wildcard_of(host);
        let route_cache = self.route_cache(host, path)
            .or_else(|| wildcard.as_deref().and_then(|w| self.route_cache(w, path)))
            .or_else(|| self.route_cache("*", path));

        if let Some(cc) = route_cache {
            return if cc.enabled { Some(cc) } else { None };
        }

        // Fall back to vhost-level config.
        let vhost_cache = self
            .cache
            .get(host)
            .or_else(|| wildcard.as_deref().and_then(|w| self.cache.get(w)))
            .or_else(|| self.cache.get("*"))?;
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
        } else if let Some((key, _)) = wildcard_of(host).and_then(|w| self.vhosts.get_key_value(w.as_str())) {
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
        self.match_route(host, path).map(|r| r.pool.as_str())
    }

    fn match_route<'a>(&'a self, host: &str, path: &str) -> Option<&'a Route> {
        let routes = self.vhosts.get(host)?;
        routes.iter().find(|r| path.starts_with(r.path_prefix.as_str()))
    }

    /// Gateway rules for the route that `resolve` would pick. `None` when no
    /// route matches or the matched route carries no rules.
    pub fn gateway(&self, host: &str, path: &str) -> Option<Arc<GatewayRules>> {
        let host = host.split(':').next().unwrap_or(host);
        let route = self
            .match_route(host, path)
            .or_else(|| wildcard_of(host).and_then(|w| self.match_route(&w, path)))
            .or_else(|| self.match_route("*", path))?;
        (!route.rules.is_empty()).then(|| Arc::clone(&route.rules))
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

/// The wildcard vhost name that would cover `host`: `*.example.com` for
/// `api.example.com`. One label only, as in certificates — `a.b.example.com`
/// is not covered by `*.example.com`, and a bare `example.com` has none.
pub fn wildcard_of(host: &str) -> Option<String> {
    let (first, rest) = host.split_once('.')?;
    if first.is_empty() || rest.is_empty() || !rest.contains('.') {
        return None;
    }
    Some(format!("*.{rest}"))
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
            acme: None,
            certificates: vec![],
            control: None,
        }
    }

    fn pool(name: &str) -> (String, Pool) {
        (name.into(), Pool {
            algorithm: LbAlgorithm::RoundRobin,
            health_check: None,
            passive: Default::default(),
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
            redirect_http: None,
            default_action: None,
            rate_limit: None,
            headers: None,
            rewrite: None,
        }
    }

    #[test]
    fn default_action_exact_pool_shadows_wildcard() {
        let mut wildcard = vhost("*", "unused");
        wildcard.pool = None;
        wildcard.default_action = Some(crate::config::DefaultAction {
            redirect: Some("https://example.com".into()),
            preserve_path: true,
            status: None,
            body: None,
        });
        let cfg = make_config(
            vec![vhost("api.example.com", "api"), wildcard],
            [pool("api")].into(),
        );
        let t = RoutingTable::build(&cfg);
        // Unknown host → wildcard default action fires.
        assert!(t.default_action("unknown.example.com").is_some());
        assert!(t.default_action("1.2.3.4:443").is_some());
        // Exact vhost with a pool routes normally — wildcard must not apply.
        assert!(t.default_action("api.example.com").is_none());
        assert_eq!(t.resolve("api.example.com", "/"), Some("api"));
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
                    CfgRoute { path: "/".into(), pool: "default".into(), cache: None, rate_limit: None, headers: None, rewrite: None },
                    CfgRoute { path: "/api/".into(), pool: "api".into(), cache: None, rate_limit: None, headers: None, rewrite: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: None,
                redirect_http: None,
                default_action: None,
                rate_limit: None,
                headers: None,
                rewrite: None,
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
                        rate_limit: None,
                        headers: None,
                        rewrite: None,
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None, rate_limit: None, headers: None, rewrite: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig {
                    enabled: true,
                    ttl: Some(60),
                    statuses: vec![],
                    content_types: vec![],
                }),
                redirect_http: None,
                default_action: None,
                rate_limit: None,
                headers: None,
                rewrite: None,
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
                        rate_limit: None,
                        headers: None,
                        rewrite: None,
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None, rate_limit: None, headers: None, rewrite: None },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig { enabled: true, ttl: Some(60), ..Default::default() }),
                redirect_http: None,
                default_action: None,
                rate_limit: None,
                headers: None,
                rewrite: None,
            }],
            [pool("api"), pool("web")].into(),
        );
        let t = RoutingTable::build(&cfg);

        // /api/ explicitly disabled — must not fall through to vhost default
        assert!(t.cache_config("example.com", "/api/users").is_none());

        // / still uses vhost default
        assert!(t.cache_config("example.com", "/").is_some());
    }

    #[test]
    fn wildcard_names() {
        assert_eq!(wildcard_of("api.example.com").as_deref(), Some("*.example.com"));
        assert_eq!(wildcard_of("a.b.example.com").as_deref(), Some("*.b.example.com"));
        assert_eq!(wildcard_of("example.com"), None);
        assert_eq!(wildcard_of("localhost"), None);
        assert_eq!(wildcard_of(".example.com"), None);
    }

    #[test]
    fn wildcard_vhost_routes_one_label_below_it() {
        let cfg = make_config(
            vec![vhost("*.example.com", "wild"), vhost("api.example.com", "api")],
            HashMap::from([pool("wild"), pool("api")]),
        );
        let table = RoutingTable::build(&cfg);
        assert_eq!(table.resolve("api.example.com", "/"), Some("api"), "exact beats wildcard");
        assert_eq!(table.resolve("www.example.com", "/"), Some("wild"));
        assert_eq!(table.resolve("www.example.com:443", "/"), Some("wild"));
        assert_eq!(table.resolve("deep.www.example.com", "/"), None, "one label only");
        assert_eq!(table.resolve("example.com", "/"), None);
        assert_eq!(table.vhost_label("www.example.com"), Some("*.example.com"));
        assert_eq!(table.vhost_label("deep.www.example.com"), None);
    }

    #[test]
    fn route_rules_override_vhost_rules_per_field() {
        let mut v = vhost("api.example.com", "unused");
        v.pool = None;
        v.rate_limit = Some(crate::config::RateLimitConfig { requests: 10, per: "1s".into(), burst: None });
        v.headers = Some(crate::config::HeaderRules::default());
        v.routes = vec![
            CfgRoute {
                path: "/v2/".into(),
                pool: "api".into(),
                cache: None,
                rate_limit: Some(crate::config::RateLimitConfig { requests: 1, per: "1s".into(), burst: None }),
                headers: None,
                rewrite: Some(crate::config::RewriteConfig { strip_prefix: Some("/v2".into()), add_prefix: None }),
            },
            CfgRoute { path: "/".into(), pool: "api".into(), cache: None, rate_limit: None, headers: None, rewrite: None },
        ];
        let cfg = make_config(vec![v, vhost("plain.example.com", "api")], HashMap::from([pool("api")]));
        let table = RoutingTable::build(&cfg);
        let v2 = table.gateway("api.example.com", "/v2/users").unwrap();
        assert_eq!(v2.id, "api.example.com/v2/");
        assert_eq!(v2.rate_limit.as_ref().unwrap().requests, 1, "route value wins");
        assert!(v2.headers.is_some(), "vhost value fills the gap");
        assert!(v2.rewrite.is_some());
        let root = table.gateway("api.example.com", "/other").unwrap();
        assert_eq!(root.rate_limit.as_ref().unwrap().requests, 10);
        assert!(root.rewrite.is_none());
        assert!(table.gateway("plain.example.com", "/").is_none(), "no rules, no allocation");
        assert!(table.gateway("nobody.example.com", "/").is_none());
    }
}
