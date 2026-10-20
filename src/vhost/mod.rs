use crate::access::AccessList;
use crate::config::{Config, DefaultAction, ForwardedHeadersConfig, VhostCacheConfig};
use std::collections::{HashMap, HashSet};

/// Maps an incoming (host, path) pair to a pool name.
///
/// Resolution order:
///  1. Exact host match, longest path-prefix match
///  2. Wildcard host ("*"), longest path-prefix match
#[derive(Default)]
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
    /// Where HTTP-01 tokens are served from; `None` when the config uses no ACME.
    acme_challenge_dir: Option<std::path::PathBuf>,
    /// host → the vhost's access list, for requests no route takes.
    access: HashMap<String, AccessList>,
    /// host → `forwarded_headers.trusted_proxies`, parsed.
    trusted: HashMap<String, Vec<ipnet::IpNet>>,
    /// `tcp` and `udp` listener address → its access list.
    listener_access: HashMap<String, AccessList>,
}

pub struct Route {
    path_prefix: String,
    pub pool: String,
    /// The route's request metrics, labelled with its pool and vhost.
    pub series: std::sync::Arc<crate::metrics::RequestSeries>,
    /// Route-level cache config — overrides the vhost-level config when present.
    cache: Option<VhostCacheConfig>,
    /// The route's access list, else its vhost's.
    pub access: Option<AccessList>,
}

impl RoutingTable {
    pub fn build(cfg: &Config) -> Self {
        let mut vhosts: HashMap<String, Vec<Route>> = HashMap::new();
        let mut forwarded: HashMap<String, ForwardedHeadersConfig> = HashMap::new();
        let mut cache: HashMap<String, VhostCacheConfig> = HashMap::new();
        let mut redirect_http: HashSet<String> = HashSet::new();
        let mut default_actions: HashMap<String, DefaultAction> = HashMap::new();
        let mut access: HashMap<String, AccessList> = HashMap::new();
        let mut trusted: HashMap<String, Vec<ipnet::IpNet>> = HashMap::new();
        // Validated: the lists parse.
        let list = |allow: &[String], deny: &[String]| AccessList::new(allow, deny).ok().flatten();

        for vhost in &cfg.vhosts {
            let routes = vhosts.entry(vhost.host.clone()).or_default();
            let vhost_access = list(&vhost.allow, &vhost.deny);

            if vhost.routes.is_empty() {
                // Single pool for the whole vhost — treat as "/"
                if let Some(pool) = &vhost.pool {
                    routes.push(Route {
                        path_prefix: "/".into(),
                        pool: pool.clone(),
                        series: std::sync::Arc::new(crate::metrics::RequestSeries::new(pool, &vhost.host)),
                        cache: None,
                        access: vhost_access.clone(),
                    });
                }
            } else {
                for r in &vhost.routes {
                    routes.push(Route {
                        path_prefix: r.path.clone(),
                        pool: r.pool.clone(),
                        series: std::sync::Arc::new(crate::metrics::RequestSeries::new(&r.pool, &vhost.host)),
                        cache: r.cache.clone(),
                        access: list(&r.allow, &r.deny).or_else(|| vhost_access.clone()),
                    });
                }
            }

            // Longest prefix first
            routes.sort_by_key(|r| std::cmp::Reverse(r.path_prefix.len()));

            if let Some(fwd) = &vhost.forwarded_headers {
                forwarded.insert(vhost.host.clone(), fwd.clone());
                let nets: Vec<_> = fwd.trusted_proxies.iter().filter_map(|n| crate::access::parse_net(n).ok()).collect();
                if !nets.is_empty() {
                    trusted.insert(vhost.host.clone(), nets);
                }
            }
            if let Some(a) = vhost_access {
                access.insert(vhost.host.clone(), a);
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

        let listener_access = cfg
            .listeners
            .iter()
            .filter_map(|l| Some((l.address.clone(), list(&l.allow, &l.deny)?)))
            .collect();
        let acme_challenge_dir = cfg.acme_effective().map(|_| crate::acme::challenge_dir(&cfg.keel));
        RoutingTable { vhosts, forwarded, cache, redirect_http, default_actions, acme_challenge_dir, access, trusted, listener_access }
    }

    /// The access list for a request: its route's, else its vhost's.
    pub fn access(&self, host: &str, path: &str) -> Option<&AccessList> {
        let vhost = self.vhost_label(host);
        // A vhost answering with its default action takes no route.
        if vhost.is_some_and(|v| self.default_actions.contains_key(v)) {
            return vhost.and_then(|v| self.access.get(v));
        }
        match self.resolve(host, path) {
            Some(route) => route.access.as_ref(),
            None => vhost.and_then(|v| self.access.get(v)),
        }
    }

    /// The vhost's trusted proxies, by vhost label.
    pub fn trusted_proxies(&self, vhost: &str) -> &[ipnet::IpNet] {
        self.trusted.get(vhost).map_or(&[], Vec::as_slice)
    }

    /// The access list of a `tcp` or `udp` listener.
    pub fn listener_access(&self, address: &str) -> Option<&AccessList> {
        self.listener_access.get(address)
    }

    pub fn acme_challenge_dir(&self) -> Option<&std::path::Path> {
        self.acme_challenge_dir.as_deref()
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

    /// Returns the route for the given host and path, or `None` if no match.
    /// Order: exact host, then its one-label wildcard (`*.example.com` for
    /// `api.example.com`), then the catch-all `"*"`.
    pub fn resolve<'a>(&'a self, host: &str, path: &str) -> Option<&'a Route> {
        let host = host.split(':').next().unwrap_or(host);
        self.match_route(host, path)
            .or_else(|| wildcard_of(host).and_then(|w| self.match_route(&w, path)))
            .or_else(|| self.match_route("*", path))
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

    fn match_route<'a>(&'a self, host: &str, path: &str) -> Option<&'a Route> {
        let routes = self.vhosts.get(host)?;
        routes.iter().find(|r| path.starts_with(r.path_prefix.as_str()))
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

    fn routed<'a>(t: &'a RoutingTable, host: &str, path: &str) -> Option<&'a str> {
        t.resolve(host, path).map(|r| r.pool.as_str())
    }

    fn make_config(vhosts: Vec<Vhost>, pools: HashMap<String, Pool>) -> Config {
        Config {
            path: String::new(),
            node_file: String::new(),
            files: Default::default(),
            keel: KeelConfig::default(),
            listeners: vec![],
            metrics: MetricsConfig::default(),
            pools,
            vhosts,
            access_log: AccessLogConfig::default(),
            cache: crate::config::CacheConfig::default(),
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
            allow: vec![],
            deny: vec![],
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
        assert_eq!(routed(&t, "api.example.com", "/"), Some("api"));
    }

    #[test]
    fn a_route_list_replaces_its_vhosts() {
        let cfg = crate::config::parse(
            "pool \"web\" {\n  backend \"10.0.0.1:80\" {}\n}\n\
             listener \"tcp\" \"0.0.0.0:22\" {\n  pool = \"web\"\n  allow = [\"10.0.0.0/8\"]\n}\n\
             vhost \"a.test\" {\n  allow = [\"10.0.0.0/8\"]\n\n  route \"/\" {\n    pool = \"web\"\n  }\n\n  \
             route \"/open/\" {\n    pool = \"web\"\n    deny = [\"192.0.2.1\"]\n  }\n}\n\
             vhost \"*\" {\n  deny = [\"10.0.0.0/8\"]\n\n  default_action {\n    status = 404\n  }\n}\n",
        )
        .unwrap();
        let t = RoutingTable::build(&cfg);
        let permits = |host: &str, path: &str, ip: &str| t.access(host, path).is_none_or(|l| l.permits(ip.parse().unwrap()));
        assert!(permits("a.test", "/", "10.1.1.1") && !permits("a.test", "/", "192.0.2.2"));
        assert!(permits("a.test", "/open/x", "192.0.2.2") && !permits("a.test", "/open/x", "192.0.2.1"));
        assert!(!permits("other.test", "/", "10.1.1.1") && permits("other.test", "/", "192.0.2.2"));
        assert!(t.listener_access("0.0.0.0:22").is_some_and(|l| !l.permits("192.0.2.2".parse().unwrap())));
    }

    #[test]
    fn exact_host_match() {
        let cfg = make_config(vec![vhost("api.example.com", "api")], [pool("api")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(routed(&t, "api.example.com", "/foo"), Some("api"));
        assert_eq!(routed(&t, "other.com", "/foo"), None);
    }

    #[test]
    fn path_prefix_longest_wins() {
        let cfg = make_config(
            vec![Vhost {
                host: "example.com".into(),
                pool: None,
                routes: vec![
                    CfgRoute { path: "/".into(), pool: "default".into(), cache: None, allow: vec![], deny: vec![] },
                    CfgRoute { path: "/api/".into(), pool: "api".into(), cache: None, allow: vec![], deny: vec![] },
                ],
                tls: None,
                forwarded_headers: None,
                cache: None,
                redirect_http: None,
                default_action: None,
                allow: vec![],
                deny: vec![],
            }],
            [pool("default"), pool("api")].into(),
        );
        let t = RoutingTable::build(&cfg);
        assert_eq!(routed(&t, "example.com", "/api/users"), Some("api"));
        assert_eq!(routed(&t, "example.com", "/static/x"), Some("default"));
    }

    #[test]
    fn wildcard_fallback() {
        let cfg = make_config(vec![vhost("*", "default")], [pool("default")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(routed(&t, "anything.com", "/"), Some("default"));
    }

    #[test]
    fn host_with_port_stripped() {
        let cfg = make_config(vec![vhost("example.com", "web")], [pool("web")].into());
        let t = RoutingTable::build(&cfg);
        assert_eq!(routed(&t, "example.com:8080", "/"), Some("web"));
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
                            ttl: Some(std::time::Duration::from_secs(3600)),
                            statuses: vec![200],
                            content_types: vec!["image/*".into()],
                        }),
                        allow: vec![],
                        deny: vec![],
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None, allow: vec![], deny: vec![] },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig {
                    enabled: true,
                    ttl: Some(std::time::Duration::from_secs(60)),
                    statuses: vec![],
                    content_types: vec![],
                }),
                redirect_http: None,
                default_action: None,
                allow: vec![],
                deny: vec![],
            }],
            [pool("assets"), pool("web")].into(),
        );
        let t = RoutingTable::build(&cfg);

        // /static/ uses route-level config (ttl 3600, image/*)
        let cc = t.cache_config("example.com", "/static/logo.png").unwrap();
        assert_eq!(cc.ttl, Some(std::time::Duration::from_secs(3600)));
        assert_eq!(cc.content_types, vec!["image/*"]);

        // / falls back to vhost-level config (ttl 60, no content_types filter)
        let cc = t.cache_config("example.com", "/").unwrap();
        assert_eq!(cc.ttl, Some(std::time::Duration::from_secs(60)));
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
                        allow: vec![],
                        deny: vec![],
                    },
                    CfgRoute { path: "/".into(), pool: "web".into(), cache: None, allow: vec![], deny: vec![] },
                ],
                tls: None,
                forwarded_headers: None,
                cache: Some(VhostCacheConfig { enabled: true, ttl: Some(std::time::Duration::from_secs(60)), ..Default::default() }),
                redirect_http: None,
                default_action: None,
                allow: vec![],
                deny: vec![],
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
        assert_eq!(routed(&table, "api.example.com", "/"), Some("api"), "exact beats wildcard");
        assert_eq!(routed(&table, "www.example.com", "/"), Some("wild"));
        assert_eq!(routed(&table, "www.example.com:443", "/"), Some("wild"));
        assert_eq!(routed(&table, "deep.www.example.com", "/"), None, "one label only");
        assert_eq!(routed(&table, "example.com", "/"), None);
        assert_eq!(table.vhost_label("www.example.com"), Some("*.example.com"));
        assert_eq!(table.vhost_label("deep.www.example.com"), None);
    }
}
