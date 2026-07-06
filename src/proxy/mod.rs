use crate::{
    access_log::{AccessLogEntry, AccessLogger},
    backend::{build_drain_entries, build_lb, LeastConnPool, Pool, PoolRegistry},
    cache::CacheHandle,
    config::{CacheConfig, Config, ForwardedMode, LbAlgorithm},
    control::ControlServer,
    health,
    metrics::MetricsService,
    tls::CertStore,
    vhost::RoutingTable,
};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use ipnet::IpNet;
use pingora::{
    cache::{
        cache_control::CacheControl,
        filters::{request_cacheable, resp_cacheable},
        CacheKey, CacheMetaDefaults,
    },
    lb::selection::{BackendIter, BackendSelection},
    proxy::{http_proxy_service, ProxyHttp, Session},
    server::Server,
    services::background::background_service,
    upstreams::peer::HttpPeer,
    Error, ErrorType,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info};

// Per-request context

pub struct ProxyCtx {
    pool: String,
    /// Canonical configured vhost label (bounded — exact host, "*", or "unmatched").
    /// Safe to use as an access-log filename component and metric label.
    vhost: String,
    /// Raw Host header from the client. Used only for the X-Forwarded-Host header.
    host_header: String,
    backend: Option<SocketAddr>,
    started_at: Instant,
    backend_started_at: Option<Instant>,
    request_bytes: usize,
    response_bytes: usize,
    client_ip: Option<IpAddr>,
    client_addr_str: Option<String>,
    is_tls: bool,
    method: String,
    uri: String,
    protocol: String,
    user_agent: Option<String>,
}

// Proxy implementation

pub struct KProxy {
    routing: Arc<ArcSwap<RoutingTable>>,
    pools: Arc<PoolRegistry>,
    access_logger: Arc<AccessLogger>,
    cache: Option<CacheHandle>,
    /// HTTP-01 challenge token directory — Some when any host is ACME-managed.
    acme_challenge_dir: Option<std::path::PathBuf>,
}

#[async_trait]
impl ProxyHttp for KProxy {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        ProxyCtx {
            pool: String::new(),
            vhost: String::new(),
            host_header: String::new(),
            backend: None,
            started_at: Instant::now(),
            backend_started_at: None,
            request_bytes: 0,
            response_bytes: 0,
            client_ip: None,
            client_addr_str: None,
            is_tls: false,
            method: String::new(),
            uri: String::new(),
            protocol: String::new(),
            user_agent: None,
        }
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora::Result<bool> {
        let is_tls = session.digest().map_or(false, |d| d.ssl_digest.is_some());

        // ACME HTTP-01 responder (plain HTTP only — the challenge arrives on
        // port 80). Must come before redirects and default actions, and works
        // for any Host — including hosts with no vhost at all (TLS-passthrough /
        // TCP domains). Serves ONLY tokens that exist in the challenge
        // directory; unknown tokens fall through to normal routing so backends
        // managing their own certificates keep working.
        if !is_tls {
            if let Some(dir) = &self.acme_challenge_dir {
                let path = session.req_header().uri.path();
                if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
                    if crate::acme::valid_token(token) {
                        if let Ok(body) = std::fs::read(dir.join(token)) {
                            let mut resp = pingora::http::ResponseHeader::build(200, Some(2))?;
                            resp.insert_header("content-type", "text/plain")?;
                            resp.insert_header("content-length", body.len().to_string())?;
                            session.write_response_header(Box::new(resp), false).await?;
                            session
                                .write_response_body(Some(bytes::Bytes::from(body)), true)
                                .await?;
                            return Ok(true);
                        }
                    }
                }
            }
        }

        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*")
            .to_owned();

        let routing = self.routing.load();

        // Default action: the matching vhost answers directly, no pool involved
        // (IP-direct redirect, unknown-host 404, maintenance page). Applies on
        // both plain HTTP and TLS.
        if let Some(action) = routing.default_action(&host) {
            if let Some(url) = &action.redirect {
                let location = if action.preserve_path {
                    let path = session
                        .req_header()
                        .uri
                        .path_and_query()
                        .map(|pq| pq.as_str())
                        .unwrap_or("/");
                    format!("{}{path}", url.trim_end_matches('/'))
                } else {
                    url.clone()
                };
                let mut resp = pingora::http::ResponseHeader::build(301, Some(2))?;
                resp.insert_header("location", location.as_str())?;
                resp.insert_header("content-length", "0")?;
                session.write_response_header(Box::new(resp), true).await?;
                return Ok(true);
            }
            if let Some(status) = action.status {
                let body = action.body.clone().unwrap_or_default();
                let mut resp = pingora::http::ResponseHeader::build(status, Some(2))?;
                resp.insert_header("content-type", "text/plain")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(bytes::Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
        }

        // HTTP → HTTPS redirect — plain HTTP only, TLS passes through.
        if is_tls || !routing.should_redirect_https(&host) {
            return Ok(false);
        }

        let path = session
            .req_header()
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let host_bare = host.split(':').next().unwrap_or(&host);
        let location = format!("https://{host_bare}{path}");

        let mut resp = pingora::http::ResponseHeader::build(301, Some(2))?;
        resp.insert_header("location", location.as_str())?;
        resp.insert_header("content-length", "0")?;
        session.write_response_header(Box::new(resp), true).await?;
        Ok(true)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let ds = session.as_downstream();

        // Capture all request metadata before any early return so logging() always has it.
        let method = ds.req_header().method.to_string();
        let uri = ds
            .req_header()
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_owned())
            .unwrap_or_else(|| ds.req_header().uri.path().to_owned());
        let protocol = format!("{:?}", ds.req_header().version);
        let user_agent = ds
            .get_header("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let host = ds
            .get_header("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("*")
            .to_owned();
        let path = ds.req_header().uri.path().to_owned();
        let client_ip: Option<IpAddr> = ds
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip());
        let client_addr_str: Option<String> = ds
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.to_string());
        let is_tls = session.digest().map_or(false, |d| d.ssl_digest.is_some());

        // Resolve the raw Host header to a bounded, operator-configured label before
        // using it as a metric label or log filename. Unmatched hosts collapse into a
        // single "unmatched" bucket — never the raw client-supplied value.
        let routing = self.routing.load();
        let vhost_label = routing
            .vhost_label(&host)
            .unwrap_or("unmatched")
            .to_owned();

        ctx.method = method;
        ctx.uri = uri;
        ctx.protocol = protocol;
        ctx.user_agent = user_agent;
        ctx.vhost = vhost_label;
        ctx.host_header = host.clone();
        ctx.client_ip = client_ip;
        ctx.client_addr_str = client_addr_str;
        ctx.is_tls = is_tls;

        // Client IP as consistent-hash key; fall back to empty bytes.
        let key: Vec<u8> = client_ip
            .map(|ip| ip.to_string().into_bytes())
            .unwrap_or_default();

        let pool_name = routing
            .resolve(&host, &path)
            .ok_or_else(|| {
                crate::metrics::record_lb_error("", &ctx.vhost, "no_route");
                Error::explain(
                    ErrorType::HTTPStatus(502),
                    format!("no vhost match for host='{host}' path='{path}'"),
                )
            })?
            .to_owned();

        // Set pool before select() so logging() can distinguish no_route from no_backend.
        ctx.pool = pool_name.clone();

        let addr = self
            .pools
            .select(&pool_name, &key)
            .ok_or_else(|| {
                crate::metrics::record_lb_error(&pool_name, &ctx.vhost, "no_backend");
                Error::explain(
                    ErrorType::HTTPStatus(502),
                    format!("pool '{pool_name}' has no available backends"),
                )
            })?;

        ctx.backend = Some(addr);
        ctx.backend_started_at = Some(Instant::now());

        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let Some(client_ip) = ctx.client_ip else {
            return Ok(());
        };

        let routing = self.routing.load();
        let fwd_cfg = routing.forwarded_config(&ctx.vhost);
        let mode = fwd_cfg.map(|c| &c.mode).unwrap_or(&ForwardedMode::Replace);

        if matches!(mode, ForwardedMode::Off) {
            upstream_request.headers.remove("x-forwarded-for");
            upstream_request.headers.remove("x-real-ip");
            upstream_request.headers.remove("x-forwarded-proto");
            upstream_request.headers.remove("x-forwarded-host");
            upstream_request.headers.remove("forwarded");
            return Ok(());
        }

        let (real_ip, xff) = if matches!(mode, ForwardedMode::Append) {
            let trusted = fwd_cfg.map(|c| c.trusted_proxies.as_slice()).unwrap_or(&[]);
            if is_trusted_proxy(&client_ip, trusted) {
                let existing = session
                    .as_downstream()
                    .get_header("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let real = existing
                    .split(',')
                    .next()
                    .and_then(|s| s.trim().parse::<IpAddr>().ok())
                    .unwrap_or(client_ip);
                let chain = if existing.is_empty() {
                    client_ip.to_string()
                } else {
                    format!("{existing}, {client_ip}")
                };
                (real, chain)
            } else {
                (client_ip, client_ip.to_string())
            }
        } else {
            (client_ip, client_ip.to_string())
        };

        let proto = if ctx.is_tls { "https" } else { "http" };
        // Forward the original client-supplied Host, not the internal label.
        let host = &ctx.host_header;

        upstream_request.insert_header("x-forwarded-for", xff.as_str())?;
        upstream_request.insert_header("x-real-ip", real_ip.to_string().as_str())?;
        upstream_request.insert_header("x-forwarded-proto", proto)?;
        upstream_request.insert_header("x-forwarded-host", host.as_str())?;
        upstream_request.insert_header(
            "forwarded",
            format!("for={real_ip};proto={proto};host={host}").as_str(),
        )?;

        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if let Some(b) = body {
            ctx.request_bytes += b.len();
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<std::time::Duration>>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(b) = body {
            ctx.response_bytes += b.len();
        }
        Ok(None)
    }

    fn request_cache_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let Some(ref cache) = self.cache else { return Ok(()); };

        if !request_cacheable(session.req_header()) {
            return Ok(());
        }

        let path = session.req_header().uri.path();
        let routing = self.routing.load();
        if routing.cache_config(&ctx.vhost, path).is_none() {
            return Ok(());
        }

        session.cache.enable(cache.storage, Some(cache.eviction), None, None, None);
        Ok(())
    }

    fn cache_key_callback(
        &self,
        session: &Session,
        _ctx: &mut Self::CTX,
    ) -> pingora::Result<CacheKey> {
        let req = session.req_header();
        let host = req
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("_");
        let path = req
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        Ok(CacheKey::new(host.as_bytes().to_vec(), path.as_bytes().to_vec(), ""))
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &pingora::http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<pingora::cache::RespCacheable> {
        use pingora::cache::{
            CacheMeta, NoCacheReason, RespCacheable::{Cacheable, Uncacheable},
        };
        use std::time::{Duration, SystemTime};

        let host = ctx.vhost.split(':').next().unwrap_or(&ctx.vhost);
        let path = session.req_header().uri.path();
        let routing = self.routing.load();

        let Some(rule) = routing.cache_config(host, path) else {
            return Ok(Uncacheable(NoCacheReason::Custom("not configured")));
        };

        // Status filter (default: 200 only)
        let status = resp.status.as_u16();
        let allowed = if rule.statuses.is_empty() { &[200u16] as &[u16] } else { &rule.statuses };
        if !allowed.contains(&status) {
            return Ok(Uncacheable(NoCacheReason::Custom("status filtered")));
        }

        // Content-type filter (empty = no restriction)
        if !rule.content_types.is_empty() {
            let ct = resp.headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            // Strip parameters ("text/html; charset=utf-8" → "text/html")
            let ct_base = ct.split(';').next().unwrap_or(ct).trim();
            let matched = rule.content_types.iter().any(|pattern| {
                if let Some(prefix) = pattern.strip_suffix('*') {
                    ct_base.starts_with(prefix)
                } else {
                    ct_base.eq_ignore_ascii_case(pattern)
                }
            });
            if !matched {
                return Ok(Uncacheable(NoCacheReason::Custom("content-type filtered")));
            }
        }

        // Cache-Control from origin
        static DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(|_| None, 0, 0);
        let cc = CacheControl::from_headers_named("cache-control", &resp.headers);
        let result = resp_cacheable(cc.as_ref(), resp.clone(), false, &DEFAULTS);

        // TTL override: apply fallback when origin sends no Cache-Control
        if matches!(result, Uncacheable(_)) {
            if let Some(ttl) = rule.ttl {
                let now = SystemTime::now();
                let meta = CacheMeta::new(
                    now + Duration::from_secs(ttl as u64),
                    now, 0, 0,
                    resp.clone(),
                );
                return Ok(Cacheable(meta));
            }
        }

        Ok(result)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut pingora::http::ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        use pingora::cache::CachePhase;
        let value = match session.cache.phase() {
            CachePhase::Hit | CachePhase::Stale | CachePhase::Revalidated => "HIT",
            CachePhase::Disabled(_) | CachePhase::Bypass => return Ok(()),
            _ => "MISS",
        };
        let _ = upstream_response.insert_header("x-cache", value);
        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        e: Option<&Error>,
        ctx: &mut Self::CTX,
    ) {
        let backend = ctx.backend.take();
        if let Some(addr) = backend {
            self.pools.release(&ctx.pool, addr);
        }

        // Backend was selected but proxying failed — count as a connection error.
        if e.is_some() {
            if let Some(addr) = backend {
                crate::metrics::record_backend_connection_error(&ctx.pool, &addr.to_string());
            }
        }

        let status = session
            .as_downstream()
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        let elapsed = ctx.started_at.elapsed().as_secs_f64();

        if !ctx.pool.is_empty() {
            crate::metrics::record_request(&ctx.pool, &ctx.vhost, status, elapsed);

            if let Some(addr) = backend {
                let backend_elapsed = ctx
                    .backend_started_at
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(elapsed);
                crate::metrics::record_backend_request(
                    &ctx.pool,
                    &addr.to_string(),
                    status,
                    backend_elapsed,
                );
            }

            crate::metrics::add_request_bytes_in(&ctx.pool, &ctx.vhost, ctx.request_bytes);
            crate::metrics::add_request_bytes_out(&ctx.pool, &ctx.vhost, ctx.response_bytes);
        }

        let error_str: Option<String> = if ctx.pool.is_empty() {
            Some("no_route".into())
        } else if backend.is_none() {
            Some("no_backend".into())
        } else if e.is_some() {
            Some("upstream_connect".into())
        } else {
            None
        };

        let backend_duration_ms = ctx
            .backend_started_at
            .map(|t| t.elapsed().as_secs_f64() * 1000.0);

        let entry = AccessLogEntry {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            method: std::mem::take(&mut ctx.method),
            uri: std::mem::take(&mut ctx.uri),
            protocol: std::mem::take(&mut ctx.protocol),
            status,
            client_addr: ctx.client_addr_str.take(),
            vhost: std::mem::take(&mut ctx.vhost),
            pool: std::mem::take(&mut ctx.pool),
            backend_addr: backend.map(|a| a.to_string()),
            bytes_in: ctx.request_bytes,
            bytes_out: ctx.response_bytes,
            duration_ms: elapsed * 1000.0,
            backend_duration_ms,
            user_agent: ctx.user_agent.take(),
            tls: ctx.is_tls,
            error: error_str,
        };

        self.access_logger.log(&entry);
    }
}

// Helpers

fn is_trusted_proxy(ip: &IpAddr, cidrs: &[String]) -> bool {
    cidrs.iter().any(|cidr| {
        cidr.parse::<IpNet>()
            .map(|net| net.contains(ip))
            .unwrap_or(false)
    })
}

// Hot reload service

struct ReloadService {
    routing: Arc<ArcSwap<RoutingTable>>,
    pools: Arc<PoolRegistry>,
    cert_store: Arc<CertStore>,
    config_path: String,
    conf_dir: Option<String>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ReloadService {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "reload: failed to register SIGHUP handler");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = sighup.recv() => {
                    match crate::config::load(&self.config_path, self.conf_dir.as_deref()) {
                        Ok(new_cfg) => {
                            self.routing.store(Arc::new(RoutingTable::build(&new_cfg)));
                            self.pools.sync_from_config(&new_cfg);
                            if let Err(e) = self.cert_store.reload(&new_cfg) {
                                error!(error = %e, "TLS cert reload failed, keeping previous certs");
                            }
                            info!(path = self.config_path, "config reloaded");
                        }
                        Err(e) => {
                            error!(
                                error = %e,
                                path = self.config_path,
                                "config reload failed, keeping previous config"
                            );
                        }
                    }
                }
            }
        }
    }
}

// Server startup

// Resolves a backend address to IP:port; pass-through if already numeric, DNS lookup if hostname.
fn resolve_addr(addr: &str) -> anyhow::Result<String> {
    if addr.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(addr.to_owned());
    }
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("cannot resolve '{addr}': {e}"))?
        .next()
        .map(|sa| sa.to_string())
        .ok_or_else(|| anyhow::anyhow!("no address resolved for '{addr}'"))
}

// Builds all pools from config, registering health-check background services on the server.
fn build_pools(cfg: &Config, server: &mut Server) -> anyhow::Result<PoolRegistry> {
    let mut pools: HashMap<String, Pool> = HashMap::new();
    let mut drain: HashMap<String, crate::backend::BackendEntry> = HashMap::new();

    for (name, pool_cfg) in &cfg.pools {
        // Resolve hostnames to IPs once at startup so Pingora gets SocketAddrs
        // and the drain map keys stay consistent with what the load balancer uses.
        let resolved: Vec<String> = pool_cfg.backends.iter()
            .map(|b| resolve_addr(&b.address))
            .collect::<anyhow::Result<_>>()?;
        let addrs: Vec<&str> = resolved.iter().map(String::as_str).collect();
        let weights: Vec<usize> = pool_cfg.backends.iter().map(|b| b.weight as usize).collect();

        build_drain_entries(name, &addrs, &mut drain);

        // Initialise drain state metric for each backend
        for addr in &addrs {
            crate::metrics::set_drain_state(name, addr, crate::backend::DRAIN_ACTIVE);
        }

        let pool = match pool_cfg.algorithm {
            LbAlgorithm::RoundRobin => {
                Pool::RoundRobin(build_with_hc::<pingora::lb::selection::RoundRobin>(
                    name, &addrs, &weights, pool_cfg, server,
                )?)
            }
            LbAlgorithm::Random => {
                Pool::Random(build_with_hc::<pingora::lb::selection::Random>(
                    name, &addrs, &weights, pool_cfg, server,
                )?)
            }
            LbAlgorithm::ConsistentHash => {
                Pool::ConsistentHash(build_with_hc::<pingora::lb::selection::Consistent>(
                    name, &addrs, &weights, pool_cfg, server,
                )?)
            }
            LbAlgorithm::LeastConnections => {
                Pool::LeastConn(Arc::new(
                    LeastConnPool::build(&addrs)
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                ))
            }
        };

        pools.insert(name.clone(), pool);
    }

    Ok(PoolRegistry::new(pools, drain))
}

// Build a typed `LoadBalancer`, optionally attach a health check and register
// a background service, then return an `Arc` to the lb.
fn build_with_hc<S>(
    name: &str,
    addrs: &[&str],
    weights: &[usize],
    pool_cfg: &crate::config::Pool,
    server: &mut Server,
) -> anyhow::Result<Arc<pingora::lb::LoadBalancer<S>>>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut lb = build_lb::<S>(addrs, weights)
        .map_err(|e| anyhow::anyhow!("pool '{name}': {e}"))?;

    if let Some(hc_cfg) = &pool_cfg.health_check {
        let hc = health::build(hc_cfg, addrs.first().copied().unwrap_or(""));
        lb.set_health_check(hc);
        lb.health_check_frequency = Some(health::parse_duration(&hc_cfg.interval));
        lb.parallel_health_check = true;

        let bg = background_service(&format!("hc:{name}"), lb);
        let arc = bg.task();
        server.add_service(bg);
        info!(pool = name, "health check enabled");
        Ok(arc)
    } else {
        Ok(Arc::new(lb))
    }
}

// Build TLS listener settings with our SNI cert resolver and a TLS 1.2 floor.
// TLS 1.0/1.1 are obsolete and must never be negotiable.
fn build_tls_settings(
    cert_store: &Arc<CertStore>,
    address: &str,
) -> pingora::listeners::tls::TlsSettings {
    let mut settings =
        pingora::listeners::tls::TlsSettings::with_callbacks(cert_store.make_callbacks())
            .unwrap_or_else(|e| {
                error!(address, error = %e, "failed to create TLS settings");
                std::process::exit(1);
            });
    if let Err(e) =
        settings.set_min_proto_version(Some(pingora::tls::ssl::SslVersion::TLS1_2))
    {
        error!(address, error = %e, "failed to set minimum TLS version");
        std::process::exit(1);
    }
    settings
}

fn log_cache_mode(cfg: &CacheConfig) {
    match (&cfg.memory, &cfg.disk) {
        (Some(mem), Some(disk)) => {
            info!(memory = mem, disk_path = disk.path, disk_size = disk.size, "cache: tiered (memory + disk) enabled");
        }
        (Some(mem), None) => {
            info!(memory = mem, "cache: memory store enabled");
        }
        (None, Some(disk)) => {
            info!(disk_path = disk.path, disk_size = disk.size, "cache: disk store enabled");
        }
        (None, None) => {}
    }
}

/// Build the Pingora server with Keel's shutdown timing. Pingora's default
/// grace period is 300s — far beyond any supervisor's kill timeout, so a
/// SIGTERM'd process would be SIGKILL'd long before exiting on its own.
fn new_server(cfg: &Config) -> Server {
    let mut conf = pingora::server::configuration::ServerConf::default();
    conf.grace_period_seconds = Some(cfg.keel.grace_period_seconds);
    conf.graceful_shutdown_timeout_seconds = Some(5);
    Server::new_with_opt_and_conf(None::<pingora::server::configuration::Opt>, conf)
}

// Start the Pingora proxy server. Never returns.
pub fn run(cfg: &Config) -> ! {
    let routing = Arc::new(ArcSwap::from_pointee(RoutingTable::build(cfg)));

    let mut server = new_server(cfg);
    server.bootstrap();

    let pools = match build_pools(cfg, &mut server) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            error!(error = %e, "failed to build pool registry");
            std::process::exit(1);
        }
    };

    let cert_store = Arc::new(CertStore::build(cfg).unwrap_or_else(|e| {
        error!(error = %e, "failed to load TLS certificates");
        std::process::exit(1);
    }));

    let access_logger = Arc::new(AccessLogger::new(&cfg.access_log));

    let cache = match crate::cache::init(&cfg.cache) {
        Ok(Some(h)) => {
            log_cache_mode(&cfg.cache);
            Some(h)
        }
        Ok(None) => None,
        Err(e) => {
            error!(error = %e, "cache: init failed, caching disabled");
            None
        }
    };

    // Register the hot-reload background service.
    server.add_service(background_service(
        "reload",
        ReloadService {
            routing: Arc::clone(&routing),
            pools: Arc::clone(&pools),
            cert_store: Arc::clone(&cert_store),
            config_path: cfg.path.clone(),
            conf_dir: cfg.conf_dir.clone(),
        },
    ));

    // ACME: register the issuance/renewal service and enable the HTTP-01
    // responder when any host is ACME-managed.
    let acme_challenge_dir = cfg
        .acme_effective()
        .map(|a| crate::acme::challenge_dir(&a.storage));
    if let Some(acme_svc) =
        crate::acme::AcmeService::from_config(cfg, Arc::clone(&cert_store), None)
    {
        server.add_service(background_service("acme", acme_svc));
    }

    add_l4_services(&mut server, cfg, &pools, &access_logger);

    let proxy =
        KProxy { routing, pools: Arc::clone(&pools), access_logger, cache, acme_challenge_dir };
    let mut svc = http_proxy_service(&server.configuration, proxy);

    let plain: Vec<_> = cfg.listeners.iter().filter(|l| !l.tls && l.tcp_pool.is_none()).collect();
    let tls: Vec<_> = cfg.listeners.iter().filter(|l| l.tls).collect();

    if cfg.listeners.is_empty() {
        info!("no listeners configured, defaulting to 0.0.0.0:8080");
        svc.add_tcp("0.0.0.0:8080");
    } else {
        for l in plain {
            info!(address = l.address, "adding listener");
            svc.add_tcp(&l.address);
        }
        for l in tls {
            let settings = build_tls_settings(&cert_store, &l.address);
            info!(address = l.address, "adding TLS listener");
            svc.add_tls_with_settings(&l.address, None, settings);
        }
    }

    server.add_service(svc);
    server.add_service(background_service("metrics", MetricsService::new(&cfg.metrics.address)));
    server.add_service(background_service(
        "control",
        ControlServer {
            socket_path: cfg.keel.control_socket.clone(),
            pools: Arc::clone(&pools),
            started_at: std::time::Instant::now(),
            cluster: None,
        },
    ));
    add_remote_control(&mut server, cfg, &pools, None);

    server.run_forever()
}

/// Register the mTLS remote control listener when `control.remote` is set.
fn add_remote_control(
    server: &mut Server,
    cfg: &Config,
    pools: &Arc<crate::backend::PoolRegistry>,
    cluster: Option<crate::cluster::ClusterHandle>,
) {
    let Some(remote) = cfg.control.as_ref().and_then(|c| c.remote.clone()) else { return };
    server.add_service(background_service(
        "control-remote",
        crate::control::remote::RemoteControlServer {
            cfg: remote,
            pools: Arc::clone(pools),
            started_at: std::time::Instant::now(),
            cluster,
        },
    ));
}

/// Register one L4 passthrough service per `tcp_pool` listener.
fn add_l4_services(
    server: &mut Server,
    cfg: &Config,
    pools: &Arc<crate::backend::PoolRegistry>,
    access_logger: &Arc<AccessLogger>,
) {
    for l in cfg.listeners.iter().filter(|l| l.tcp_pool.is_some()) {
        let pool = l.tcp_pool.clone().unwrap();
        info!(address = l.address, pool, "adding TCP passthrough listener");
        let app = crate::l4::TcpProxyApp {
            listener: l.address.clone(),
            pool,
            pools: Arc::clone(pools),
            access_logger: Arc::clone(access_logger),
        };
        let mut svc = pingora::services::listening::Service::new(
            format!("tcp-{}", l.address),
            app,
        );
        svc.add_tcp(&l.address);
        server.add_service(svc);
    }
}

// Cluster reload watcher

struct ClusterReloadWatcher {
    routing: Arc<ArcSwap<RoutingTable>>,
    pools: Arc<PoolRegistry>,
    cert_store: Arc<CertStore>,
    config_rx: tokio::sync::watch::Receiver<Option<String>>,
}

#[async_trait]
impl pingora::services::background::BackgroundService for ClusterReloadWatcher {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        let mut rx = self.config_rx.clone();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                result = rx.changed() => {
                    if result.is_err() { return; }
                    if let Some(yaml) = rx.borrow().clone() {
                        match serde_yml::from_str::<crate::config::Config>(&yaml) {
                            Ok(new_cfg) => {
                                self.routing.store(Arc::new(RoutingTable::build(&new_cfg)));
                                self.pools.sync_from_config(&new_cfg);
                                if let Err(e) = self.cert_store.reload(&new_cfg) {
                                    error!(error = %e, "cluster: TLS cert reload failed");
                                }
                                info!("cluster: config applied from Raft log");
                            }
                            Err(e) => error!(error = %e, "cluster: invalid config YAML"),
                        }
                    }
                }
            }
        }
    }
}

// Start the proxy in cluster mode: registers ClusterService as a background service
// and watches Raft-committed config changes. Never returns.
pub fn run_cluster(
    cfg: &Config,
    cluster: crate::cluster::ClusterHandle,
    cluster_svc: crate::cluster::ClusterService,
) -> ! {
    let routing = Arc::new(ArcSwap::from_pointee(RoutingTable::build(cfg)));

    let mut server = new_server(cfg);
    server.bootstrap();

    let pools = match build_pools(cfg, &mut server) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            error!(error = %e, "failed to build pool registry");
            std::process::exit(1);
        }
    };

    let cert_store = Arc::new(CertStore::build(cfg).unwrap_or_else(|e| {
        error!(error = %e, "failed to load TLS certificates");
        std::process::exit(1);
    }));

    let access_logger = Arc::new(AccessLogger::new(&cfg.access_log));

    let cache = match crate::cache::init(&cfg.cache) {
        Ok(Some(h)) => {
            log_cache_mode(&cfg.cache);
            Some(h)
        }
        Ok(None) => None,
        Err(e) => {
            error!(error = %e, "cache: init failed, caching disabled");
            None
        }
    };

    server.add_service(background_service("cluster", cluster_svc));
    server.add_service(background_service(
        "cluster-reload",
        ClusterReloadWatcher {
            routing: Arc::clone(&routing),
            pools: Arc::clone(&pools),
            cert_store: Arc::clone(&cert_store),
            config_rx: cluster.config_rx.clone(),
        },
    ));
    server.add_service(background_service(
        "reload",
        ReloadService {
            routing: Arc::clone(&routing),
            pools: Arc::clone(&pools),
            cert_store: Arc::clone(&cert_store),
            config_path: cfg.path.clone(),
            conf_dir: cfg.conf_dir.clone(),
        },
    ));

    let acme_challenge_dir = cfg
        .acme_effective()
        .map(|a| crate::acme::challenge_dir(&a.storage));
    if let Some(acme_svc) = crate::acme::AcmeService::from_config(
        cfg,
        Arc::clone(&cert_store),
        Some(cluster.clone()),
    ) {
        server.add_service(background_service("acme", acme_svc));
    }

    add_l4_services(&mut server, cfg, &pools, &access_logger);

    let proxy =
        KProxy { routing, pools: Arc::clone(&pools), access_logger, cache, acme_challenge_dir };
    let mut svc = http_proxy_service(&server.configuration, proxy);

    let plain: Vec<_> = cfg.listeners.iter().filter(|l| !l.tls && l.tcp_pool.is_none()).collect();
    let tls_listeners: Vec<_> = cfg.listeners.iter().filter(|l| l.tls).collect();

    if cfg.listeners.is_empty() {
        info!("no listeners configured, defaulting to 0.0.0.0:8080");
        svc.add_tcp("0.0.0.0:8080");
    } else {
        for l in plain {
            info!(address = l.address, "adding listener");
            svc.add_tcp(&l.address);
        }
        for l in tls_listeners {
            let settings = build_tls_settings(&cert_store, &l.address);
            info!(address = l.address, "adding TLS listener");
            svc.add_tls_with_settings(&l.address, None, settings);
        }
    }

    server.add_service(svc);
    server.add_service(background_service("metrics", MetricsService::new(&cfg.metrics.address)));
    add_remote_control(&mut server, cfg, &pools, Some(cluster.clone()));
    server.add_service(background_service(
        "control",
        ControlServer {
            socket_path: cfg.keel.control_socket.clone(),
            pools: Arc::clone(&pools),
            started_at: std::time::Instant::now(),
            cluster: Some(cluster),
        },
    ));

    server.run_forever()
}
