use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub fn load(path: &str, conf_dir: Option<&str>) -> Result<Config> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("cannot read {path}"))?;
    let mut cfg: Config = serde_yml::from_str(&raw)
        .with_context(|| format!("cannot parse {path}"))?;
    cfg.path = path.to_owned();
    cfg.conf_dir = conf_dir.map(|s| s.to_owned());

    // Collect glob patterns: from include: in YAML plus optional --conf-dir flag.
    let mut patterns = cfg.include.clone();
    if let Some(dir) = conf_dir {
        patterns.push(format!("{dir}/**/*.yaml"));
    }

    if !patterns.is_empty() {
        let root_canonical = fs::canonicalize(path).ok().unwrap_or_else(|| PathBuf::from(path));

        // Expand all patterns, deduplicate, sort alphabetically.
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut fragment_paths: Vec<PathBuf> = Vec::new();
        for pattern in &patterns {
            let entries = glob::glob(pattern)
                .with_context(|| format!("invalid glob pattern '{pattern}'"))?;
            for entry in entries {
                let p = entry.with_context(|| format!("glob error in pattern '{pattern}'"))?;
                if seen.insert(p.clone()) {
                    fragment_paths.push(p);
                }
            }
        }
        fragment_paths.sort();

        for frag_path in &fragment_paths {
            // Skip the root config file itself if it matches a glob.
            let frag_canonical = fs::canonicalize(frag_path)
                .ok()
                .unwrap_or_else(|| frag_path.clone());
            if frag_canonical == root_canonical {
                continue;
            }

            let frag_str = frag_path.to_string_lossy();
            let raw = fs::read_to_string(frag_path)
                .with_context(|| format!("cannot read {frag_str}"))?;

            check_no_root_sections(&frag_str, &raw)?;

            let fragment: IncludeFragment = serde_yml::from_str(&raw)
                .with_context(|| format!("cannot parse {frag_str}"))?;

            // Pools merge as a map — duplicate name is an error.
            for (name, pool) in fragment.pools {
                if cfg.pools.contains_key(&name) {
                    anyhow::bail!(
                        "{frag_str}: pool '{name}' is already defined in another config file"
                    );
                }
                cfg.pools.insert(name, pool);
            }

            // Vhosts, listeners, and certificates are appended in load order.
            cfg.vhosts.extend(fragment.vhosts);
            cfg.listeners.extend(fragment.listeners);
            cfg.certificates.extend(fragment.certificates);
        }
    }

    cfg.validate()?;
    Ok(cfg)
}

/// Reject conf.d files that contain root-only sections.
fn check_no_root_sections(path: &str, raw: &str) -> Result<()> {
    let value: serde_yml::Value = serde_yml::from_str(raw)
        .with_context(|| format!("cannot parse {path}"))?;

    let forbidden = ["keel", "metrics", "access_log", "include", "cluster", "acme", "control"];
    if let Some(map) = value.as_mapping() {
        for key in &forbidden {
            if map.contains_key(*key) {
                anyhow::bail!(
                    "{path}: conf.d files may not contain '{key}' (root-level section only)"
                );
            }
        }
    }
    Ok(())
}

/// Fragment parsed from a conf.d include file.
#[derive(Debug, Deserialize, Default)]
struct IncludeFragment {
    #[serde(default)]
    pools: HashMap<String, Pool>,

    #[serde(default)]
    vhosts: Vec<Vhost>,

    #[serde(default)]
    listeners: Vec<Listener>,

    #[serde(default)]
    certificates: Vec<CertificateRequest>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Path this config was loaded from — set by load(), not from YAML.
    #[serde(skip, default)]
    pub path: String,

    /// --conf-dir CLI value — set by load(), not from YAML.
    #[serde(skip, default)]
    pub conf_dir: Option<String>,

    #[serde(default)]
    pub keel: KeelConfig,

    #[serde(default)]
    pub listeners: Vec<Listener>,

    #[serde(default)]
    pub metrics: MetricsConfig,

    #[serde(default)]
    pub pools: HashMap<String, Pool>,

    #[serde(default)]
    pub vhosts: Vec<Vhost>,

    #[serde(default)]
    pub access_log: AccessLogConfig,

    #[serde(default)]
    pub cache: CacheConfig,

    /// Glob patterns for conf.d include files. Processed at load time.
    #[serde(default)]
    pub include: Vec<String>,

    #[serde(default)]
    pub cluster: Option<ClusterConfig>,

    #[serde(default)]
    pub acme: Option<AcmeConfig>,

    /// Standalone certificate requests (no vhost). Merged from conf.d files
    /// like vhosts are.
    #[serde(default)]
    pub certificates: Vec<CertificateRequest>,

    /// Remote control plane (keelctl). Off unless `control.remote` is set.
    #[serde(default)]
    pub control: Option<ControlConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ControlConfig {
    pub remote: Option<RemoteControlConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteControlConfig {
    /// TCP listen address for keelctl connections (mTLS required).
    pub address: String,

    /// Optional source-CIDR restriction, checked before the TLS handshake.
    /// Empty = allow any source. Narrows exposure; mTLS stays mandatory.
    #[serde(default)]
    pub allow: Vec<String>,

    /// Directory holding the control CA (ca.crt / ca.key). Generated on
    /// first use; `keel credentials create` signs client certs with it.
    #[serde(default = "default_control_ca_dir")]
    pub ca_dir: String,
}

pub fn default_control_ca_dir() -> String { "/var/lib/keel/control".into() }

impl Config {
    fn validate(&self) -> Result<()> {
        // Must be a literal address, not a name: the master binds it inline
        // so that resolving it cannot add a thread to the process that forks
        // workers (see control::remote and process::run_master).
        if let Some(remote) = self.control.as_ref().and_then(|c| c.remote.as_ref()) {
            if remote.address.parse::<std::net::SocketAddr>().is_err() {
                anyhow::bail!(
                    "control.remote.address must be a literal ip:port (e.g. 0.0.0.0:10789), got '{}'",
                    remote.address
                );
            }
        }

        for (name, pool) in &self.pools {
            if pool.backends.is_empty() {
                anyhow::bail!("pool '{name}' has no backends");
            }
            if pool.passive.failures == 0 {
                anyhow::bail!("pool '{name}': passive.failures must be at least 1");
            }
            parse_duration(&pool.passive.eject_for)
                .map_err(|e| anyhow::anyhow!("pool '{name}': passive.eject_for: {e}"))?;
            if let Some(hc) = &pool.health_check {
                parse_duration(&hc.interval)
                    .map_err(|e| anyhow::anyhow!("pool '{name}': health_check.interval: {e}"))?;
                parse_duration(&hc.timeout)
                    .map_err(|e| anyhow::anyhow!("pool '{name}': health_check.timeout: {e}"))?;
                if hc.healthy_threshold == 0 || hc.unhealthy_threshold == 0 {
                    anyhow::bail!("pool '{name}': health_check thresholds must be at least 1");
                }
                if hc.port == Some(0) {
                    anyhow::bail!("pool '{name}': health_check.port must be 1-65535");
                }
                match &hc.probe {
                    ProbeConfig::Http { path, expect_status, .. } => {
                        if !path.starts_with('/') {
                            anyhow::bail!("pool '{name}': health_check.path must start with '/'");
                        }
                        if let Some(s) = expect_status.iter().find(|s| !(100..=599).contains(*s)) {
                            anyhow::bail!("pool '{name}': health_check.expect_status {s} is not an HTTP status");
                        }
                    }
                    ProbeConfig::Tls { sni: Some(sni), .. } => {
                        let labels: Vec<&str> = sni.split('.').collect();
                        if sni.is_empty() || labels.iter().any(|l| l.is_empty() || l.len() > 63) {
                            anyhow::bail!("pool '{name}': health_check.sni '{sni}' is not a valid host name");
                        }
                    }
                    ProbeConfig::Dns { query, record, expect, .. } => {
                        let labels: Vec<&str> = query.trim_end_matches('.').split('.').collect();
                        if query.is_empty() || labels.iter().any(|l| l.is_empty() || l.len() > 63) || query.len() > 253 {
                            anyhow::bail!("pool '{name}': health_check.query '{query}' is not a valid DNS name");
                        }
                        if let Some(e) = expect {
                            let ip: std::net::IpAddr = e
                                .parse()
                                .map_err(|_| anyhow::anyhow!("pool '{name}': health_check.expect '{e}' is not an IP address"))?;
                            let family_ok = matches!((record, ip), (DnsRecord::A, std::net::IpAddr::V4(_)) | (DnsRecord::Aaaa, std::net::IpAddr::V6(_)));
                            if !family_ok {
                                anyhow::bail!("pool '{name}': health_check.expect '{e}' does not match record type {record:?}");
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(remote) = self.control.as_ref().and_then(|c| c.remote.as_ref()) {
            for cidr in &remote.allow {
                cidr.parse::<ipnet::IpNet>().map_err(|e| {
                    anyhow::anyhow!("control.remote.allow: invalid CIDR '{cidr}': {e}")
                })?;
            }
        }
        if self.keel.udp_flow_timeout_seconds == 0 {
            anyhow::bail!("keel.udp_flow_timeout_seconds must be at least 1");
        }
        for l in &self.listeners {
            if l.proxy_protocol && l.tls {
                anyhow::bail!(
                    "listener '{}': proxy_protocol is not supported on tls listeners — the header \
                     precedes the TLS handshake, which Pingora completes before Keel sees the connection",
                    l.address
                );
            }
            if let Some(pool) = &l.tcp_pool {
                if !self.pools.contains_key(pool) {
                    anyhow::bail!("listener '{}' references unknown tcp_pool '{pool}'", l.address);
                }
                if l.tls {
                    anyhow::bail!(
                        "listener '{}': tcp_pool is passthrough — remove 'tls' \
                         (TLS termination for TCP pools is not implemented)",
                        l.address
                    );
                }
            }
            if l.tcp_pool.is_none() && (l.tls_mode != TlsMode::Passthrough || l.tls_host.is_some()) {
                anyhow::bail!("listener '{}': tls_mode and tls_host apply to tcp_pool listeners only", l.address);
            }
            if l.tcp_pool.is_some() {
                match l.tls_mode {
                    TlsMode::Passthrough => {
                        if l.tls_host.is_some() || l.tls_verify || l.tls_ca.is_some() {
                            anyhow::bail!(
                                "listener '{}': tls_host, tls_verify, and tls_ca need tls_mode terminate or reencrypt",
                                l.address
                            );
                        }
                    }
                    TlsMode::Terminate | TlsMode::Reencrypt => {
                        let Some(host) = &l.tls_host else {
                            anyhow::bail!(
                                "listener '{}': tls_mode {:?} needs tls_host (a certificates: entry or a vhost with tls)",
                                l.address, l.tls_mode
                            );
                        };
                        let known = self.certificates.iter().any(|c| &c.host == host)
                            || self.vhosts.iter().any(|v| &v.host == host && v.tls.is_some());
                        if !known {
                            anyhow::bail!(
                                "listener '{}': tls_host '{host}' matches no certificates: entry or vhost with tls",
                                l.address
                            );
                        }
                        if l.tls_mode == TlsMode::Terminate && (l.tls_verify || l.tls_ca.is_some()) {
                            anyhow::bail!("listener '{}': tls_verify and tls_ca apply to tls_mode reencrypt only", l.address);
                        }
                        if l.tls_ca.is_some() && !l.tls_verify {
                            anyhow::bail!("listener '{}': tls_ca needs tls_verify: true", l.address);
                        }
                    }
                }
            }
            if let Some(pool) = &l.udp_pool {
                if !self.pools.contains_key(pool) {
                    anyhow::bail!("listener '{}' references unknown udp_pool '{pool}'", l.address);
                }
                if l.tls {
                    anyhow::bail!(
                        "listener '{}': udp_pool cannot be combined with 'tls' \
                         (UDP datagrams are forwarded as-is, never terminated)",
                        l.address
                    );
                }
                if l.tcp_pool.is_some() {
                    anyhow::bail!(
                        "listener '{}': udp_pool and tcp_pool are mutually exclusive — \
                         use one listener entry per protocol",
                        l.address
                    );
                }
            }
        }
        for vhost in &self.vhosts {
            if let Some(action) = &vhost.default_action {
                if vhost.pool.is_some() || !vhost.routes.is_empty() {
                    anyhow::bail!(
                        "vhost '{}': default_action excludes pool and routes",
                        vhost.host
                    );
                }
                match (&action.redirect, action.status) {
                    (Some(_), Some(_)) => anyhow::bail!(
                        "vhost '{}': default_action takes redirect OR status, not both",
                        vhost.host
                    ),
                    (None, None) => anyhow::bail!(
                        "vhost '{}': default_action requires redirect or status",
                        vhost.host
                    ),
                    (Some(url), None) => {
                        if !url.starts_with("http://") && !url.starts_with("https://") {
                            anyhow::bail!(
                                "vhost '{}': default_action.redirect must be an absolute http(s) URL",
                                vhost.host
                            );
                        }
                        if action.body.is_some() {
                            anyhow::bail!(
                                "vhost '{}': default_action.body is only valid with status",
                                vhost.host
                            );
                        }
                    }
                    (None, Some(s)) => {
                        if !(100..=599).contains(&s) {
                            anyhow::bail!(
                                "vhost '{}': default_action.status {s} is not a valid HTTP status",
                                vhost.host
                            );
                        }
                    }
                }
            }
            validate_gateway(&format!("vhost '{}'", vhost.host), vhost.rate_limit.as_ref(), vhost.headers.as_ref(), vhost.rewrite.as_ref(), vhost.auth.as_ref())?;
            for route in &vhost.routes {
                validate_gateway(&format!("vhost '{}' route '{}'", vhost.host, route.path), route.rate_limit.as_ref(), route.headers.as_ref(), route.rewrite.as_ref(), route.auth.as_ref())?;
            }
            if let Some(pool) = &vhost.pool {
                if !self.pools.contains_key(pool) {
                    anyhow::bail!("vhost '{}' references unknown pool '{pool}'", vhost.host);
                }
            }
            for route in &vhost.routes {
                if !self.pools.contains_key(&route.pool) {
                    anyhow::bail!(
                        "vhost '{}' route '{}' references unknown pool '{}'",
                        vhost.host, route.path, route.pool
                    );
                }
            }
            if let Some(tls) = &vhost.tls {
                if tls.acme.enabled() {
                    if tls.cert.is_some() || tls.key.is_some() {
                        anyhow::bail!(
                            "vhost '{}': tls.acme is set — remove cert/key (they are managed in the ACME storage dir)",
                            vhost.host
                        );
                    }
                    if vhost.host.contains('*') && !self.issuer_uses_dns01(tls.acme.issuer_name().unwrap_or(DEFAULT_ISSUER)) {
                        anyhow::bail!(
                            "vhost '{}': wildcard certificates need an issuer with challenge: dns-01",
                            vhost.host
                        );
                    }
                } else if tls.cert.is_none() || tls.key.is_none() {
                    anyhow::bail!(
                        "vhost '{}': tls requires cert and key paths (or acme)",
                        vhost.host
                    );
                }
            }
        }
        for c in &self.certificates {
            if c.cert.is_some() != c.key.is_some() {
                anyhow::bail!("certificates '{}': cert and key must be set together", c.host);
            }
        }
        self.validate_acme()?;
        Ok(())
    }

    /// Whether the named issuer publishes DNS-01 challenges (the implicit
    /// default issuer does not).
    fn issuer_uses_dns01(&self, name: &str) -> bool {
        self.acme
            .as_ref()
            .and_then(|a| a.issuers.get(name))
            .map_or(false, |i| i.challenge == AcmeChallenge::Dns01)
    }

    fn validate_acme(&self) -> Result<()> {
        let issuer_defined = |name: &str| {
            self.acme.as_ref().map_or(false, |a| a.issuers.contains_key(name))
        };

        for (name, issuer) in self.acme.as_ref().map(|a| &a.issuers).into_iter().flatten() {
            match (issuer.challenge, &issuer.dns) {
                (AcmeChallenge::Dns01, None) => anyhow::bail!("acme.issuers.{name}: challenge dns-01 needs a dns provider"),
                (AcmeChallenge::Http01, Some(_)) => anyhow::bail!("acme.issuers.{name}: dns provider needs challenge: dns-01"),
                (AcmeChallenge::Dns01, Some(DnsProvider::Rfc2136 { server, zone, tsig_name, tsig_key, tsig_key_file, tsig_algorithm, propagation_wait, .. })) => {
                    if server.rsplit_once(':').map_or(true, |(h, p)| h.is_empty() || p.parse::<u16>().is_err()) {
                        anyhow::bail!("acme.issuers.{name}: dns.server must be host:port");
                    }
                    if zone.is_empty() || tsig_name.is_empty() {
                        anyhow::bail!("acme.issuers.{name}: dns.zone and dns.tsig_name are required");
                    }
                    if tsig_key.is_some() == tsig_key_file.is_some() {
                        anyhow::bail!("acme.issuers.{name}: set exactly one of dns.tsig_key and dns.tsig_key_file");
                    }
                    if crate::acme::rfc2136::TsigAlgorithm::parse(tsig_algorithm).is_none() {
                        anyhow::bail!("acme.issuers.{name}: dns.tsig_algorithm must be hmac-sha256, hmac-sha384, or hmac-sha512");
                    }
                    parse_duration(propagation_wait).map_err(|e| anyhow::anyhow!("acme.issuers.{name}: dns.propagation_wait: {e}"))?;
                }
                (AcmeChallenge::Http01, None) => {}
            }
        }
        for c in self.certificates.iter().filter(|c| c.is_acme()) {
            if c.host.contains('*') && !self.issuer_uses_dns01(&c.issuer) {
                anyhow::bail!("certificates '{}': wildcard certificates need an issuer with challenge: dns-01", c.host);
            }
        }

        // Issuer references must exist ("default" may be implicit).
        for vhost in &self.vhosts {
            let Some(name) = vhost.tls.as_ref().and_then(|t| t.acme.issuer_name()) else {
                continue;
            };
            if !issuer_defined(name) && name != DEFAULT_ISSUER {
                anyhow::bail!(
                    "vhost '{}': tls.acme references issuer '{name}', which is not defined under acme.issuers",
                    vhost.host
                );
            }
        }
        for cert in self.certificates.iter().filter(|c| c.is_acme()) {
            if !issuer_defined(&cert.issuer) && cert.issuer != DEFAULT_ISSUER {
                anyhow::bail!(
                    "certificates '{}': references issuer '{}', which is not defined under acme.issuers",
                    cert.host, cert.issuer
                );
            }
            if cert.host.contains('*') {
                anyhow::bail!(
                    "certificates '{}': ACME HTTP-01 cannot issue wildcard certificates",
                    cert.host
                );
            }
        }

        if let Some(acme) = &self.acme {
            RenewBefore::parse(&acme.renew_before)
                .with_context(|| format!("acme.renew_before '{}'", acme.renew_before))?;
            for (name, issuer) in &acme.issuers {
                if let Some(rb) = &issuer.renew_before {
                    RenewBefore::parse(rb).with_context(|| {
                        format!("acme.issuers.{name}.renew_before '{rb}'")
                    })?;
                }
            }
        }

        // Each hostname has exactly one certificate — one issuer. Check the raw
        // config (not the deduplicated assignments) so conflicts surface.
        let mut owner: HashMap<&str, &str> = HashMap::new();
        let mut claims: Vec<(&str, &str)> = Vec::new();
        for v in &self.vhosts {
            if let Some(name) = v.tls.as_ref().and_then(|t| t.acme.issuer_name()) {
                claims.push((v.host.as_str(), name));
            }
        }
        for c in self.certificates.iter().filter(|c| c.is_acme()) {
            claims.push((c.host.as_str(), c.issuer.as_str()));
        }
        for (host, issuer) in claims {
            if let Some(prev) = owner.insert(host, issuer) {
                if prev != issuer {
                    anyhow::bail!(
                        "host '{host}' is assigned to both ACME issuer '{prev}' and '{issuer}' — a host can only have one certificate"
                    );
                }
            }
        }
        Ok(())
    }

    /// The effective ACME config with the implicit "default" issuer
    /// materialized when referenced but not defined. `None` when ACME is
    /// unused entirely.
    pub fn acme_effective(&self) -> Option<AcmeConfig> {
        let assignments = self.acme_assignments();
        if assignments.is_empty() {
            return None;
        }
        let mut acme = self.acme.clone().unwrap_or_default();
        for (_, issuer, _) in &assignments {
            if !acme.issuers.contains_key(*issuer) {
                // validate() guarantees only "default" can be undefined.
                acme.issuers.insert((*issuer).to_owned(), AcmeIssuer::default());
            }
        }
        Some(acme)
    }

    /// Every ACME-managed hostname as `(host, issuer, vhost_managed)`.
    /// `vhost_managed` is true when Keel terminates TLS for the host (the cert
    /// goes into the CertStore) and false for standalone `certificates:`
    /// entries (cert files only). Deduplicated, order preserved.
    pub fn acme_assignments(&self) -> Vec<(&str, &str, bool)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for v in &self.vhosts {
            if let Some(name) = v.tls.as_ref().and_then(|t| t.acme.issuer_name()) {
                if seen.insert(v.host.as_str()) {
                    out.push((v.host.as_str(), name, true));
                }
            }
        }
        for c in self.certificates.iter().filter(|c| c.is_acme()) {
            if seen.insert(c.host.as_str()) {
                out.push((c.host.as_str(), c.issuer.as_str(), false));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeelConfig {
    #[serde(default = "default_workers")]
    pub workers: usize,

    #[serde(default = "default_user")]
    pub user: String,

    #[serde(default = "default_group")]
    pub group: String,

    #[serde(default = "default_control_socket")]
    pub control_socket: String,

    /// Seconds to let in-flight requests finish on graceful shutdown before
    /// the process exits. Keep below the supervisor's kill timeout
    /// (docker stop default 10s, K8s terminationGracePeriodSeconds 30s).
    #[serde(default = "default_grace_period")]
    pub grace_period_seconds: u64,

    /// Seconds a UDP flow (one client ip:port on a `udp_pool` listener) may
    /// stay idle before it expires and releases its backend. Must be >= 1.
    #[serde(default = "default_udp_flow_timeout")]
    pub udp_flow_timeout_seconds: u64,
}

impl Default for KeelConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            user: default_user(),
            group: default_group(),
            control_socket: default_control_socket(),
            grace_period_seconds: default_grace_period(),
            udp_flow_timeout_seconds: default_udp_flow_timeout(),
        }
    }
}

fn default_grace_period() -> u64 { 10 }
fn default_udp_flow_timeout() -> u64 { 30 }

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}
fn default_user() -> String { "keel".into() }
fn default_group() -> String { "keel".into() }
fn default_control_socket() -> String { "/var/run/keel/keel.sock".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Listener {
    pub address: String,

    #[serde(default)]
    pub tls: bool,

    /// Expect a PROXY protocol v1/v2 header on every connection (TCP, HTTP)
    /// or datagram (UDP) and take the client address from it. Connections
    /// without one are rejected. Not available on `tls: true` listeners.
    #[serde(default)]
    pub proxy_protocol: bool,

    /// L4 mode: splice raw TCP to this pool (passthrough — the stream is
    /// never inspected, TLS is end-to-end between client and backend).
    /// Mutually exclusive with `tls` (termination) and HTTP routing.
    #[serde(default)]
    pub tcp_pool: Option<String>,

    /// L4 mode: forward UDP datagrams to this pool. One flow per client
    /// ip:port, pinned to a backend until idle for
    /// `keel.udp_flow_timeout_seconds`. Mutually exclusive with `tls` and
    /// `tcp_pool`.
    #[serde(default)]
    pub udp_pool: Option<String>,

    /// TLS handling on a `tcp_pool` listener.
    #[serde(default)]
    pub tls_mode: TlsMode,

    /// Certificate served by a terminating listener: the host of a
    /// `certificates:` entry or of a vhost with `tls`. Used when the client
    /// sends no SNI or an SNI with no certificate of its own.
    #[serde(default)]
    pub tls_host: Option<String>,

    /// reencrypt: verify the backend certificate against the system trust
    /// store plus `tls_ca`. Off by default — wire encryption, not backend
    /// authentication.
    #[serde(default)]
    pub tls_verify: bool,

    /// reencrypt with `tls_verify`: PEM bundle of additional trusted CAs.
    #[serde(default)]
    pub tls_ca: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Bytes spliced untouched; TLS, if any, is end to end.
    #[default]
    Passthrough,
    /// Keel terminates TLS, plaintext to the backend.
    Terminate,
    /// Keel terminates TLS and opens a new TLS connection to the backend.
    Reencrypt,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_address")]
    pub address: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { address: default_metrics_address() }
    }
}

// Localhost by default — metrics expose backend addresses, pool/vhost names, and
// traffic volumes. Operators that scrape remotely set 0.0.0.0 explicitly and are
// expected to firewall the port (or run a local agent scraping 127.0.0.1).
fn default_metrics_address() -> String { "127.0.0.1:9090".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pool {
    #[serde(default)]
    pub algorithm: LbAlgorithm,

    pub health_check: Option<HealthCheck>,

    /// Passive detection from real traffic; on by default.
    #[serde(default)]
    pub passive: PassiveConfig,

    #[serde(default)]
    pub backends: Vec<Backend>,
}

/// Outlier ejection: a backend whose real requests, connections, or UDP
/// flows fail `failures` times in a row is taken out for `eject_for`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PassiveConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_passive_failures")]
    pub failures: u32,

    #[serde(default = "default_passive_eject_for")]
    pub eject_for: String,
}

impl Default for PassiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failures: default_passive_failures(),
            eject_for: default_passive_eject_for(),
        }
    }
}

fn default_passive_failures() -> u32 { 5 }
fn default_passive_eject_for() -> String { "30s".into() }

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LbAlgorithm {
    #[default]
    RoundRobin,
    Random,
    LeastConnections,
    ConsistentHash,
}

/// Active health check for a pool. `type` selects the probe; the fields a
/// probe accepts are checked strictly on load (see `Deserialize` below), so
/// a `path` on a `tcp` check is an error rather than silently ignored.
#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    #[serde(flatten)]
    pub probe: ProbeConfig,

    pub interval: String,
    pub timeout: String,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,

    /// Probe this port instead of the backend's traffic port (admin or
    /// health ports). Applies to every probe type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProbeConfig {
    /// TCP connect succeeds.
    Tcp,
    /// Empty datagram sent; healthy unless ICMP port-unreachable comes back.
    Udp,
    /// HTTP GET; 2xx (or `expect_status`) and optionally `expect_body`.
    Http {
        #[serde(default = "default_health_path")]
        path: String,
        /// Host header and TLS SNI; defaults to the backend address.
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        tls: bool,
        /// Acceptable status codes; empty means any 2xx.
        #[serde(default)]
        expect_status: Vec<u16>,
        /// Substring that must appear in the response body.
        #[serde(default)]
        expect_body: Option<String>,
    },
    /// One DNS query; healthy on NOERROR, optionally with `expect` among the answers.
    Dns {
        /// Name to resolve.
        query: String,
        #[serde(default)]
        record: DnsRecord,
        #[serde(default)]
        transport: DnsTransport,
        /// Address that must appear in the answer section.
        #[serde(default)]
        expect: Option<String>,
    },
    /// NTP client request; healthy on a server reply with non-zero stratum.
    Ntp,
    /// ICMP echo to the backend host; proves the host, not the service.
    Icmp,
    /// TLS handshake completes; optionally the certificate stays valid for
    /// at least `min_days_valid` days. The chain is not verified.
    Tls {
        /// SNI to send; default: the backend IP (no SNI).
        #[serde(default)]
        sni: Option<String>,
        #[serde(default)]
        min_days_valid: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum DnsRecord {
    #[default]
    A,
    Aaaa,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DnsTransport {
    #[default]
    Udp,
    Tcp,
}

/// Every probe type name, for error messages.
pub const PROBE_TYPES: &[&str] = &["tcp", "udp", "http", "dns", "ntp", "icmp", "tls"];

const HEALTH_COMMON_FIELDS: &[&str] =
    &["type", "interval", "timeout", "healthy_threshold", "unhealthy_threshold", "port"];

/// Fields a probe type accepts besides the common ones; `None` = unknown type.
fn probe_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "tcp" | "udp" | "ntp" | "icmp" => &[],
        "http" => &["path", "host", "tls", "expect_status", "expect_body"],
        "dns" => &["query", "record", "transport", "expect"],
        "tls" => &["sni", "min_days_valid"],
        _ => return None,
    })
}

/// Shape serde parses; `HealthCheck` wraps it after the strict key check.
#[derive(Deserialize)]
struct RawHealthCheck {
    #[serde(flatten)]
    probe: ProbeConfig,
    #[serde(default = "default_health_interval")]
    interval: String,
    #[serde(default = "default_health_timeout")]
    timeout: String,
    #[serde(default = "default_healthy_threshold")]
    healthy_threshold: u32,
    #[serde(default = "default_unhealthy_threshold")]
    unhealthy_threshold: u32,
    #[serde(default)]
    port: Option<u16>,
}

impl<'de> Deserialize<'de> for HealthCheck {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_yml::Value::deserialize(d)?;
        let map = value
            .as_mapping()
            .ok_or_else(|| D::Error::custom("health_check must be a mapping"))?;
        let kind = map
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| D::Error::custom("health_check.type is required"))?
            .to_owned();
        let extra = probe_fields(&kind).ok_or_else(|| {
            D::Error::custom(format!(
                "health_check.type '{kind}' is not one of: {}",
                PROBE_TYPES.join(", ")
            ))
        })?;
        for key in map.keys() {
            let k = key.as_str();
            if !HEALTH_COMMON_FIELDS.contains(&k) && !extra.contains(&k) {
                return Err(D::Error::custom(format!(
                    "health_check: field '{k}' is not valid for type '{kind}'"
                )));
            }
        }
        let raw: RawHealthCheck = serde_yml::from_value(value).map_err(D::Error::custom)?;
        Ok(HealthCheck {
            probe: raw.probe,
            interval: raw.interval,
            timeout: raw.timeout,
            healthy_threshold: raw.healthy_threshold,
            unhealthy_threshold: raw.unhealthy_threshold,
            port: raw.port,
        })
    }
}

fn validate_gateway(
    what: &str,
    rate_limit: Option<&RateLimitConfig>,
    headers: Option<&HeaderRules>,
    rewrite: Option<&RewriteConfig>,
    auth: Option<&AuthConfig>,
) -> Result<()> {
    if let Some(a) = auth {
        let j = &a.jwt;
        let sources = [j.secret.is_some(), j.secret_file.is_some(), j.public_key.is_some()].iter().filter(|b| **b).count();
        if sources != 1 {
            anyhow::bail!("{what}: auth.jwt needs exactly one of secret, secret_file, public_key");
        }
        if let Some(s) = &j.secret {
            use base64::Engine;
            if base64::engine::general_purpose::STANDARD.decode(s.trim()).is_err() {
                anyhow::bail!("{what}: auth.jwt.secret is not base64");
            }
        }
        for (field, path) in [("secret_file", &j.secret_file), ("public_key", &j.public_key)] {
            if let Some(p) = path {
                std::fs::metadata(p).map_err(|e| anyhow::anyhow!("{what}: auth.jwt.{field} '{p}': {e}"))?;
            }
        }
        if http::header::HeaderName::from_bytes(j.header.as_bytes()).is_err() {
            anyhow::bail!("{what}: auth.jwt.header is not a valid header name");
        }
        parse_duration(&j.leeway).map_err(|e| anyhow::anyhow!("{what}: auth.jwt.leeway: {e}"))?;
        for (claim, header) in &j.claim_headers {
            if claim.is_empty() || http::header::HeaderName::from_bytes(header.as_bytes()).is_err() {
                anyhow::bail!("{what}: auth.jwt.claim_headers: '{header}' is not a valid header name for claim '{claim}'");
            }
        }
    }
    if let Some(rl) = rate_limit {
        if rl.requests == 0 {
            anyhow::bail!("{what}: rate_limit.requests must be at least 1");
        }
        parse_duration(&rl.per).map_err(|e| anyhow::anyhow!("{what}: rate_limit.per: {e}"))?;
        if rl.burst == Some(0) {
            anyhow::bail!("{what}: rate_limit.burst must be at least 1");
        }
    }
    if let Some(h) = headers {
        for ops in [&h.request, &h.response] {
            for (name, value) in &ops.set {
                if http::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                    anyhow::bail!("{what}: headers: '{name}' is not a valid header name");
                }
                if http::header::HeaderValue::from_str(value).is_err() {
                    anyhow::bail!("{what}: headers: value of '{name}' contains characters not allowed in a header");
                }
            }
            for name in &ops.remove {
                if http::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                    anyhow::bail!("{what}: headers: '{name}' is not a valid header name");
                }
            }
        }
    }
    if let Some(rw) = rewrite {
        for (field, value) in [("strip_prefix", &rw.strip_prefix), ("add_prefix", &rw.add_prefix)] {
            if let Some(v) = value {
                if !v.starts_with('/') || v.contains(['?', '#']) {
                    anyhow::bail!("{what}: rewrite.{field} must be a path starting with '/'");
                }
            }
        }
        if rw.strip_prefix.is_none() && rw.add_prefix.is_none() {
            anyhow::bail!("{what}: rewrite needs strip_prefix or add_prefix");
        }
    }
    Ok(())
}

/// Strict duration parser for config values: `500ms`, `10s`, `2m`, `1h`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("'{s}' is not a duration (use e.g. 500ms, 10s, 2m)"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => anyhow::bail!("'{s}' is not a duration (use e.g. 500ms, 10s, 2m)"),
    };
    if d.is_zero() {
        anyhow::bail!("'{s}': duration must be greater than zero");
    }
    Ok(d)
}

fn default_health_path() -> String { "/health".into() }
fn default_health_interval() -> String { "10s".into() }
fn default_health_timeout() -> String { "2s".into() }
fn default_healthy_threshold() -> u32 { 2 }
fn default_unhealthy_threshold() -> u32 { 3 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Backend {
    pub address: String,

    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 { 1 }

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ForwardedMode {
    /// Always set headers to direct client IP. Prevents spoofing.
    #[default]
    Replace,
    /// Preserve chain from trusted proxies, append direct client IP.
    Append,
    /// Remove all forwarded headers from upstream request.
    Off,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForwardedHeadersConfig {
    #[serde(default)]
    pub mode: ForwardedMode,
    /// CIDR ranges of trusted upstream proxies (used in Append mode).
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CacheConfig {
    /// Memory budget, e.g. "256M", "1G". Memory cache disabled if absent.
    pub memory: Option<String>,
    /// Disk cache config. Disk cache disabled if absent.
    pub disk: Option<DiskCacheConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiskCacheConfig {
    /// Directory for cache files.
    pub path: String,
    /// Disk budget, e.g. "500M", "10G".
    pub size: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VhostCacheConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Fallback TTL in seconds when origin sends no Cache-Control.
    pub ttl: Option<u32>,
    /// HTTP status codes to cache. Defaults to [200] when empty.
    #[serde(default)]
    pub statuses: Vec<u16>,
    /// Content-type prefixes to cache. Empty means no restriction.
    /// Supports trailing wildcard: "image/*" matches "image/png", "image/jpeg", etc.
    #[serde(default)]
    pub content_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Vhost {
    pub host: String,

    pub pool: Option<String>,

    #[serde(default)]
    pub routes: Vec<Route>,

    pub tls: Option<TlsConfig>,

    pub forwarded_headers: Option<ForwardedHeadersConfig>,

    pub cache: Option<VhostCacheConfig>,

    /// Redirect plain HTTP requests to HTTPS with a 301.
    /// Defaults to true when `tls.acme: true` (override with an explicit false);
    /// must be set explicitly for BYO certs.
    #[serde(default)]
    pub redirect_http: Option<bool>,

    /// Answer requests directly without a backend pool: a 301 redirect to an
    /// absolute URL, or a static status/body. Mutually exclusive with `pool`
    /// and `routes`. Typical on the `"*"` wildcard vhost: IP-direct access
    /// redirect, unknown-host 404, maintenance page.
    pub default_action: Option<DefaultAction>,

    /// Gateway rules for the whole vhost; a route's own rules override them
    /// field by field.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub headers: Option<HeaderRules>,
    #[serde(default)]
    pub rewrite: Option<RewriteConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

/// Authentication before a request may reach the backend.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AuthConfig {
    pub jwt: JwtConfig,
}

/// Self-contained JWT validation: the key is configured, nothing is fetched.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JwtConfig {
    /// Base64 shared secret for HS256/384/512 — or `secret_file` holding it.
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub secret_file: Option<String>,
    /// PEM public key (SPKI) for RS256/384/512 or ES256/384.
    #[serde(default)]
    pub public_key: Option<String>,
    /// Required `iss` claim value.
    #[serde(default)]
    pub issuer: Option<String>,
    /// Value the `aud` claim must equal or contain.
    #[serde(default)]
    pub audience: Option<String>,
    /// Header carrying `Bearer <token>`.
    #[serde(default = "default_jwt_header")]
    pub header: String,
    /// Clock skew tolerated on `exp` and `nbf`.
    #[serde(default = "default_jwt_leeway")]
    pub leeway: String,
    /// claim name → request header to carry it to the backend.
    #[serde(default)]
    pub claim_headers: std::collections::BTreeMap<String, String>,
}

fn default_jwt_header() -> String { "Authorization".into() }
fn default_jwt_leeway() -> String { "30s".into() }

impl JwtConfig {
    /// Decoded shared secret, from `secret` or `secret_file`; `None` when a
    /// public key is configured instead.
    pub fn secret_material(&self) -> Result<Option<Vec<u8>>> {
        use base64::Engine;
        let b64 = match (&self.secret, &self.secret_file) {
            (Some(s), _) => s.trim().to_owned(),
            (None, Some(path)) => std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read secret_file {path}: {e}"))?
                .trim()
                .to_owned(),
            (None, None) => return Ok(None),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .map_err(|_| anyhow::anyhow!("jwt secret is not base64"))?;
        Ok(Some(bytes))
    }
}

/// Per-client-IP token bucket: `requests` per `per`, with `burst` tokens
/// available after idle time (defaults to `requests`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RateLimitConfig {
    pub requests: u32,
    #[serde(default = "default_rate_per")]
    pub per: String,
    #[serde(default)]
    pub burst: Option<u32>,
}

fn default_rate_per() -> String { "1s".into() }

impl RateLimitConfig {
    /// (tokens per second, bucket capacity). `per` is validated at load.
    pub fn limit(&self) -> (f64, f64) {
        let per = parse_duration(&self.per).unwrap_or(Duration::from_secs(1)).as_secs_f64();
        let rate = self.requests as f64 / per;
        (rate, self.burst.unwrap_or(self.requests) as f64)
    }
}

/// Header changes on the way to the backend (`request`) and back to the
/// client (`response`).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct HeaderRules {
    #[serde(default)]
    pub request: HeaderOps,
    #[serde(default)]
    pub response: HeaderOps,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct HeaderOps {
    /// Set (replace) these headers to static values.
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    /// Remove these headers.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Path rewriting toward the backend.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct RewriteConfig {
    /// Remove this leading path segment when present (`/api` turns
    /// `/api/users` into `/users`).
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// Prepend this path segment.
    #[serde(default)]
    pub add_prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultAction {
    /// Absolute URL to 301 to. Path + query are appended unless
    /// `preserve_path: false`.
    pub redirect: Option<String>,
    #[serde(default = "default_true")]
    pub preserve_path: bool,
    /// Static response status (e.g. 404, 503).
    pub status: Option<u16>,
    /// Static response body; only valid with `status`.
    pub body: Option<String>,
}

fn default_true() -> bool { true }

impl Vhost {
    /// Effective HTTP→HTTPS redirect: explicit value wins; ACME vhosts default
    /// to true (the challenge path bypasses the redirect in the proxy).
    pub fn redirect_http_effective(&self) -> bool {
        self.redirect_http
            .unwrap_or_else(|| self.tls.as_ref().map_or(false, |t| t.acme.enabled()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    pub path: String,
    pub pool: String,
    /// Per-route cache config. Overrides the vhost-level cache config when set.
    pub cache: Option<VhostCacheConfig>,

    /// Route-level gateway rules; each overrides the vhost's when set.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub headers: Option<HeaderRules>,
    #[serde(default)]
    pub rewrite: Option<RewriteConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Certificate path (BYO cert). Not set when ACME is enabled.
    pub cert: Option<String>,
    /// Private key path (BYO cert). Not set when ACME is enabled.
    pub key: Option<String>,
    /// Automatic certificates via ACME: `true` uses the issuer named
    /// `default`; a string names an issuer from `acme.issuers`.
    #[serde(default)]
    pub acme: AcmeRef,
}

/// `tls.acme` accepts a bool (`true` → issuer "default") or an issuer name.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AcmeRef {
    Enabled(bool),
    Issuer(String),
}

impl Default for AcmeRef {
    fn default() -> Self {
        AcmeRef::Enabled(false)
    }
}

impl AcmeRef {
    /// The issuer this vhost uses, or `None` when ACME is off.
    pub fn issuer_name(&self) -> Option<&str> {
        match self {
            AcmeRef::Enabled(false) => None,
            AcmeRef::Enabled(true) => Some(DEFAULT_ISSUER),
            AcmeRef::Issuer(name) => Some(name.as_str()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.issuer_name().is_some()
    }
}

/// Issuer name that `tls: { acme: true }` refers to.
pub const DEFAULT_ISSUER: &str = "default";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcmeConfig {
    /// Directory for certificates, keys, accounts, and challenge tokens.
    #[serde(default = "default_acme_storage")]
    pub storage: String,

    /// When to renew: a percentage of the certificate's total lifetime
    /// remaining ("30%", works for both 90-day and short-lived certs) or an
    /// absolute window ("20d"). Per-issuer override available.
    #[serde(default = "default_renew_before")]
    pub renew_before: String,

    /// Named certificate issuers. Vhosts reference them via `tls.acme`.
    /// `tls: { acme: true }` means the issuer named "default", which is
    /// implicitly Let's Encrypt production when not defined here.
    #[serde(default)]
    pub issuers: HashMap<String, AcmeIssuer>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            storage: default_acme_storage(),
            renew_before: default_renew_before(),
            issuers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcmeIssuer {
    /// Contact email for this issuer's ACME account (expiry warnings).
    pub email: Option<String>,

    /// ACME v2 directory URL. Defaults to Let's Encrypt production.
    #[serde(default = "default_acme_directory")]
    pub directory: String,

    /// PEM file with an additional trust root for the ACME directory itself.
    /// Only needed for internal CAs or testing against Pebble.
    pub root_ca: Option<String>,

    /// Per-issuer renewal override; same syntax as the global `renew_before`.
    pub renew_before: Option<String>,

    /// Which challenge this issuer's orders use. `dns-01` needs `dns` and
    /// allows wildcard hosts.
    #[serde(default)]
    pub challenge: AcmeChallenge,

    /// DNS provider for `dns-01`.
    #[serde(default)]
    pub dns: Option<DnsProvider>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq)]
pub enum AcmeChallenge {
    #[default]
    #[serde(rename = "http-01")]
    Http01,
    #[serde(rename = "dns-01")]
    Dns01,
}

/// How DNS-01 TXT records are published.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DnsProvider {
    /// RFC 2136 dynamic update with TSIG, to the zone's primary.
    Rfc2136 {
        /// `host:port` of the primary that accepts updates.
        server: String,
        /// Zone the challenge names belong to.
        zone: String,
        /// TSIG key name, as in the server's key statement.
        tsig_name: String,
        /// Base64 key material; or `tsig_key_file` holding it.
        #[serde(default)]
        tsig_key: Option<String>,
        #[serde(default)]
        tsig_key_file: Option<String>,
        #[serde(default = "default_tsig_algorithm")]
        tsig_algorithm: String,
        /// Time to wait after the record is visible on the primary, for
        /// secondaries the CA may query.
        #[serde(default = "default_propagation_wait")]
        propagation_wait: String,
        #[serde(default = "default_dns_ttl")]
        ttl: u32,
    },
}

fn default_tsig_algorithm() -> String { "hmac-sha256".into() }
fn default_propagation_wait() -> String { "10s".into() }
fn default_dns_ttl() -> u32 { 60 }

impl Default for AcmeIssuer {
    fn default() -> Self {
        Self {
            email: None,
            directory: default_acme_directory(),
            root_ca: None,
            renew_before: None,
            challenge: AcmeChallenge::Http01,
            dns: None,
        }
    }
}

/// A standalone certificate request: a hostname Keel obtains a certificate
/// for without terminating TLS for it (plain TCP / TLS-passthrough backends).
/// Keel answers the HTTP-01 challenge; the cert/key files land in
/// `acme.storage` for the operator or backend to consume (Lego
/// standalone-style). Lives at the top level so conf.d files can declare
/// their own, next to their vhosts and pools.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CertificateRequest {
    pub host: String,
    /// Issuer name from `acme.issuers`. Defaults to "default". Ignored when
    /// `cert`/`key` are set.
    #[serde(default = "default_issuer_name")]
    pub issuer: String,

    /// Bring-your-own certificate: PEM paths instead of ACME issuance.
    #[serde(default)]
    pub cert: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

impl CertificateRequest {
    /// Managed by ACME (as opposed to loaded from `cert`/`key` files).
    pub fn is_acme(&self) -> bool {
        self.cert.is_none()
    }
}

fn default_issuer_name() -> String { DEFAULT_ISSUER.into() }

/// Parsed `renew_before` value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenewBefore {
    /// Renew when less than this percentage of total lifetime remains.
    Percent(u8),
    /// Renew when fewer than this many days remain.
    Days(u32),
}

impl RenewBefore {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            let pct: u8 = p.trim().parse().context("renew_before percentage")?;
            if !(1..=90).contains(&pct) {
                anyhow::bail!("renew_before percentage must be 1–90, got {pct}%");
            }
            Ok(RenewBefore::Percent(pct))
        } else if let Some(d) = s.strip_suffix('d') {
            let days: u32 = d.trim().parse().context("renew_before days")?;
            if days == 0 {
                anyhow::bail!("renew_before days must be at least 1");
            }
            Ok(RenewBefore::Days(days))
        } else {
            anyhow::bail!("renew_before must be a percentage ('30%') or days ('20d'), got '{s}'")
        }
    }
}

fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_acme_storage() -> String { "/var/lib/keel/acme".into() }
fn default_renew_before() -> String { "30%".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccessLogConfig {
    #[serde(default = "default_access_log_enabled")]
    pub enabled: bool,

    #[serde(default = "default_access_log_dir")]
    pub dir: String,
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self { enabled: default_access_log_enabled(), dir: default_access_log_dir() }
    }
}

fn default_access_log_enabled() -> bool { true }
fn default_access_log_dir() -> String { "/var/log/keel".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClusterConfig {
    /// Peer RPC listen address. Default: 0.0.0.0:7654
    #[serde(default = "default_cluster_addr")]
    pub addr: String,

    /// This node's Raft node ID. Derived from addr hash if not set.
    pub node_id: Option<u64>,

    pub secret: Option<String>,
    pub ca_cert: Option<String>,
    pub ca_key: Option<String>,
}

fn default_cluster_addr() -> String { "0.0.0.0:7654".into() }

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "pools:\n  dns:\n    backends:\n      - address: 10.0.0.1:53\n";

    fn parse(listeners: &str) -> Config {
        serde_yml::from_str(&format!("{POOL}listeners:\n{listeners}")).expect("yaml parses")
    }

    fn err(cfg: &Config) -> String {
        cfg.validate().expect_err("validation should fail").to_string()
    }

    #[test]
    fn remote_control_address_must_be_literal() {
        // A hostname would resolve through tokio's blocking pool, adding a
        // thread to the process that forks workers.
        let yaml = "control:\n  remote:\n    address: localhost:10789\npools:\n  p:\n    backends:\n      - address: 10.0.0.1:80\n";
        let err = serde_yml::from_str::<Config>(yaml).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("literal ip:port"), "unexpected error: {err}");

        let yaml = "control:\n  remote:\n    address: 0.0.0.0:10789\npools:\n  p:\n    backends:\n      - address: 10.0.0.1:80\n";
        assert!(serde_yml::from_str::<Config>(yaml).unwrap().validate().is_ok());
    }

    #[test]
    fn udp_pool_listener_is_accepted() {
        let cfg = parse("  - address: 0.0.0.0:53\n    udp_pool: dns\n");
        cfg.validate().expect("valid");
        assert_eq!(cfg.listeners[0].udp_pool.as_deref(), Some("dns"));
        assert_eq!(cfg.keel.udp_flow_timeout_seconds, 30);
    }

    #[test]
    fn udp_pool_must_name_an_existing_pool() {
        let cfg = parse("  - address: 0.0.0.0:53\n    udp_pool: resolvers\n");
        assert!(err(&cfg).contains("unknown udp_pool 'resolvers'"));
    }

    #[test]
    fn udp_pool_rejects_tls() {
        let cfg = parse("  - address: 0.0.0.0:853\n    udp_pool: dns\n    tls: true\n");
        assert!(err(&cfg).contains("udp_pool cannot be combined with 'tls'"));
    }

    #[test]
    fn udp_pool_rejects_tcp_pool_on_same_listener() {
        let cfg = parse("  - address: 0.0.0.0:53\n    udp_pool: dns\n    tcp_pool: dns\n");
        assert!(err(&cfg).contains("udp_pool and tcp_pool are mutually exclusive"));
    }

    #[test]
    fn udp_and_tcp_on_separate_listeners_share_a_pool() {
        let cfg = parse(
            "  - address: 0.0.0.0:53\n    udp_pool: dns\n  - address: 0.0.0.0:53\n    tcp_pool: dns\n",
        );
        cfg.validate().expect("valid");
    }

    fn pool_with(hc: &str) -> Config {
        let yaml = format!("pools:\n  p:\n    health_check:\n{hc}    backends:\n      - address: 10.0.0.1:80\n");
        serde_yml::from_str(&yaml).expect("yaml parses")
    }

    fn parse_err(hc: &str) -> String {
        let yaml = format!("pools:\n  p:\n    health_check:\n{hc}    backends:\n      - address: 10.0.0.1:80\n");
        serde_yml::from_str::<Config>(&yaml).expect_err("should not parse").to_string()
    }

    #[test]
    fn health_check_tcp_and_http_keep_their_shape() {
        let cfg = pool_with("      type: tcp\n      interval: 5s\n");
        let hc = cfg.pools["p"].health_check.as_ref().unwrap();
        assert!(matches!(hc.probe, ProbeConfig::Tcp));
        assert_eq!(hc.interval, "5s");
        assert_eq!(hc.timeout, "2s");
        assert_eq!(hc.healthy_threshold, 2);
        cfg.validate().expect("valid");

        let cfg = pool_with("      type: http\n      path: /ready\n      expect_status: [200, 204]\n      port: 9000\n");
        let hc = cfg.pools["p"].health_check.as_ref().unwrap();
        match &hc.probe {
            ProbeConfig::Http { path, expect_status, tls, .. } => {
                assert_eq!(path, "/ready");
                assert_eq!(expect_status, &[200, 204]);
                assert!(!tls);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hc.port, Some(9000));
        cfg.validate().expect("valid");
    }

    #[test]
    fn health_check_rejects_fields_of_other_types() {
        assert!(parse_err("      type: tcp\n      path: /health\n").contains("'path' is not valid for type 'tcp'"));
        assert!(parse_err("      type: udp\n      expect_body: ok\n").contains("'expect_body' is not valid for type 'udp'"));
        assert!(parse_err("      type: smtp\n").contains("not one of"));
        assert!(parse_err("      interval: 5s\n").contains("type is required"));
    }

    #[test]
    fn dns_and_ntp_probes_parse_and_validate() {
        let cfg = pool_with("      type: dns\n      query: example.com\n      record: AAAA\n      transport: tcp\n      expect: '2001:db8::1'\n");
        match &cfg.pools["p"].health_check.as_ref().unwrap().probe {
            ProbeConfig::Dns { query, record, transport, expect } => {
                assert_eq!(query, "example.com");
                assert_eq!(*record, DnsRecord::Aaaa);
                assert_eq!(*transport, DnsTransport::Tcp);
                assert_eq!(expect.as_deref(), Some("2001:db8::1"));
            }
            other => panic!("unexpected {other:?}"),
        }
        cfg.validate().expect("valid");
        let cfg = pool_with("      type: ntp\n");
        assert!(matches!(cfg.pools["p"].health_check.as_ref().unwrap().probe, ProbeConfig::Ntp));
        cfg.validate().expect("valid");
        let cfg = pool_with("      type: icmp\n      timeout: 500ms\n");
        assert!(matches!(cfg.pools["p"].health_check.as_ref().unwrap().probe, ProbeConfig::Icmp));
        cfg.validate().expect("valid");
        let cfg = pool_with("      type: tls\n      sni: db.example.com\n      min_days_valid: 14\n");
        match &cfg.pools["p"].health_check.as_ref().unwrap().probe {
            ProbeConfig::Tls { sni, min_days_valid } => {
                assert_eq!(sni.as_deref(), Some("db.example.com"));
                assert_eq!(*min_days_valid, Some(14));
            }
            other => panic!("unexpected {other:?}"),
        }
        cfg.validate().expect("valid");
        assert!(pool_with("      type: tls\n      sni: 'a..b'\n").validate().unwrap_err().to_string().contains("not a valid host name"));
        assert!(parse_err("      type: tls\n      path: /\n").contains("'path' is not valid for type 'tls'"));

        let err = |hc: &str| pool_with(hc).validate().expect_err("invalid").to_string();
        assert!(err("      type: dns\n      query: ''\n").contains("not a valid DNS name"));
        assert!(err("      type: dns\n      query: a..b\n").contains("not a valid DNS name"));
        assert!(err("      type: dns\n      query: example.com\n      expect: nope\n").contains("not an IP address"));
        assert!(err("      type: dns\n      query: example.com\n      expect: '::1'\n").contains("does not match record type"));
        assert!(parse_err("      type: dns\n").contains("query"));
        assert!(parse_err("      type: ntp\n      query: x\n").contains("'query' is not valid for type 'ntp'"));
    }

    #[test]
    fn passive_defaults_and_validation() {
        let cfg: Config = serde_yml::from_str("pools:\n  p:\n    backends:\n      - address: 10.0.0.1:80\n").unwrap();
        let p = &cfg.pools["p"].passive;
        assert!(p.enabled);
        assert_eq!(p.failures, 5);
        assert_eq!(p.eject_for, "30s");
        cfg.validate().expect("valid");

        let cfg: Config = serde_yml::from_str("pools:\n  p:\n    passive:\n      enabled: false\n      failures: 0\n    backends:\n      - address: 10.0.0.1:80\n").unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("passive.failures"));
        let cfg: Config = serde_yml::from_str("pools:\n  p:\n    passive:\n      eject_for: forever\n    backends:\n      - address: 10.0.0.1:80\n").unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("passive.eject_for"));
    }

    #[test]
    fn health_check_validates_values() {
        let err = |hc: &str| pool_with(hc).validate().expect_err("invalid").to_string();
        assert!(err("      type: tcp\n      interval: soon\n").contains("health_check.interval"));
        assert!(err("      type: tcp\n      timeout: 0s\n").contains("greater than zero"));
        assert!(err("      type: tcp\n      healthy_threshold: 0\n").contains("thresholds"));
        assert!(err("      type: http\n      path: health\n").contains("start with '/'"));
        assert!(err("      type: http\n      expect_status: [42]\n").contains("not an HTTP status"));
    }

    #[test]
    fn durations_parse_strictly() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("fast").is_err());
        assert!(parse_duration("0s").is_err());
    }

    fn tcp_tls(listener_fields: &str, certificates: &str) -> Config {
        serde_yml::from_str(&format!(
            "{POOL}listeners:\n  - address: 0.0.0.0:6379\n    tcp_pool: dns\n{listener_fields}certificates:\n{certificates}"
        ))
        .expect("yaml parses")
    }

    #[test]
    fn tcp_tls_modes_validate() {
        let byo = "  - host: cache.example.com\n    cert: /c.pem\n    key: /k.pem\n";
        tcp_tls("    tls_mode: terminate\n    tls_host: cache.example.com\n", byo).validate().expect("terminate ok");
        tcp_tls("    tls_mode: reencrypt\n    tls_host: cache.example.com\n    tls_verify: true\n    tls_ca: /ca.pem\n", byo)
            .validate()
            .expect("reencrypt ok");
        let cfg = tcp_tls("", byo);
        assert_eq!(cfg.listeners[0].tls_mode, TlsMode::Passthrough);
        assert!(!cfg.certificates[0].is_acme());
        cfg.validate().expect("passthrough default ok");

        let e = |l: &str, c: &str| tcp_tls(l, c).validate().expect_err("invalid").to_string();
        assert!(e("    tls_mode: terminate\n", byo).contains("needs tls_host"));
        assert!(e("    tls_mode: terminate\n    tls_host: other.example.com\n", byo).contains("matches no certificates"));
        assert!(e("    tls_host: cache.example.com\n", byo).contains("need tls_mode terminate or reencrypt"));
        assert!(e("    tls_mode: terminate\n    tls_host: cache.example.com\n    tls_verify: true\n", byo).contains("reencrypt only"));
        assert!(e("    tls_mode: reencrypt\n    tls_host: cache.example.com\n    tls_ca: /ca.pem\n", byo).contains("needs tls_verify"));
        assert!(e("", "  - host: cache.example.com\n    cert: /c.pem\n").contains("set together"));
        let cfg = parse("  - address: 0.0.0.0:80\n    tls_mode: terminate\n");
        assert!(err(&cfg).contains("tcp_pool listeners only"));
    }

    fn acme_cfg(issuer_body: &str, host: &str) -> Config {
        serde_yml::from_str(&format!(
            "{POOL}acme:\n  issuers:\n    internal:\n{issuer_body}vhosts:\n  - host: '{host}'\n    pool: dns\n    tls: {{ acme: internal }}\n"
        ))
        .expect("yaml parses")
    }

    #[test]
    fn dns01_issuer_parses_and_allows_wildcards() {
        let dns = "      challenge: dns-01\n      dns:\n        type: rfc2136\n        server: ns1.example.test:53\n        zone: example.test\n        tsig_name: keel-acme\n        tsig_key: c2VjcmV0\n";
        let cfg = acme_cfg(dns, "*.example.test");
        cfg.validate().expect("wildcard with dns-01 is valid");
        let issuer = &cfg.acme.as_ref().unwrap().issuers["internal"];
        assert_eq!(issuer.challenge, AcmeChallenge::Dns01);
        match &issuer.dns {
            Some(DnsProvider::Rfc2136 { tsig_algorithm, propagation_wait, ttl, .. }) => {
                assert_eq!(tsig_algorithm, "hmac-sha256");
                assert_eq!(propagation_wait, "10s");
                assert_eq!(*ttl, 60);
            }
            other => panic!("unexpected {other:?}"),
        }

        let e = |body: &str, host: &str| acme_cfg(body, host).validate().expect_err("invalid").to_string();
        assert!(e("      email: a@example.test\n", "*.example.test").contains("need an issuer with challenge: dns-01"));
        assert!(e("      challenge: dns-01\n", "a.example.test").contains("needs a dns provider"));
        assert!(e("      dns:\n        type: rfc2136\n        server: ns1:53\n        zone: z\n        tsig_name: k\n        tsig_key: a\n", "a.example.test").contains("needs challenge: dns-01"));
        assert!(e(&dns.replace("        tsig_key: c2VjcmV0\n", ""), "a.example.test").contains("exactly one of"));
        assert!(e(&dns.replace("ns1.example.test:53", "ns1.example.test"), "a.example.test").contains("host:port"));
        assert!(e(&dns.replace("        tsig_key: c2VjcmV0\n", "        tsig_key: c2VjcmV0\n        tsig_algorithm: hmac-md5\n"), "a.example.test").contains("tsig_algorithm"));
    }

    fn gw(vhost_body: &str) -> Config {
        serde_yml::from_str(&format!("{POOL}vhosts:\n  - host: api.example.test\n    pool: dns\n{vhost_body}")).expect("yaml parses")
    }

    #[test]
    fn gateway_blocks_parse_and_validate() {
        let cfg = gw("    rate_limit: { requests: 10, per: 1m, burst: 20 }\n    headers:\n      request: { set: { X-Env: prod }, remove: [X-Internal] }\n      response: { remove: [Server] }\n    rewrite: { strip_prefix: /api }\n");
        cfg.validate().expect("valid");
        let v = &cfg.vhosts[0];
        assert_eq!(v.rate_limit.as_ref().unwrap().limit(), (10.0 / 60.0, 20.0));
        assert_eq!(v.headers.as_ref().unwrap().request.set["X-Env"], "prod");
        assert_eq!(v.rewrite.as_ref().unwrap().strip_prefix.as_deref(), Some("/api"));
        assert_eq!(gw("    rate_limit: { requests: 5 }\n").vhosts[0].rate_limit.as_ref().unwrap().limit(), (5.0, 5.0));

        let e = |body: &str| gw(body).validate().expect_err("invalid").to_string();
        assert!(e("    rate_limit: { requests: 0 }\n").contains("requests must be at least 1"));
        assert!(e("    rate_limit: { requests: 1, per: often }\n").contains("rate_limit.per"));
        assert!(e("    headers: { request: { set: { 'Bad Name': x } } }\n").contains("not a valid header name"));
        assert!(e("    headers: { response: { set: { X-A: \"a\\nb\" } } }\n").contains("not allowed in a header"));
        assert!(e("    rewrite: { strip_prefix: api }\n").contains("starting with '/'"));
        assert!(e("    rewrite: {}\n").contains("needs strip_prefix or add_prefix"));

        let cfg = gw("    auth:\n      jwt: { secret: c2VjcmV0, issuer: https://i.test, claim_headers: { sub: X-Auth-Subject } }\n");
        cfg.validate().expect("valid jwt");
        let j = &cfg.vhosts[0].auth.as_ref().unwrap().jwt;
        assert_eq!(j.header, "Authorization");
        assert_eq!(j.leeway, "30s");
        assert_eq!(j.secret_material().unwrap().unwrap(), b"secret");
        assert!(e("    auth:\n      jwt: {}\n").contains("exactly one of"));
        assert!(e("    auth:\n      jwt: { secret: a, public_key: /k.pem }\n").contains("exactly one of"));
        assert!(e("    auth:\n      jwt: { secret: '***' }\n").contains("not base64"));
        assert!(e("    auth:\n      jwt: { secret: c2VjcmV0, claim_headers: { sub: 'bad name' } }\n").contains("claim_headers"));
        assert!(e("    auth:\n      jwt: { public_key: /nonexistent/jwt.pem }\n").contains("auth.jwt.public_key"));
    }

    #[test]
    fn proxy_protocol_allowed_except_on_tls_listeners() {
        parse("  - address: 0.0.0.0:80\n    proxy_protocol: true\n").validate().expect("http listener");
        parse("  - address: 0.0.0.0:53\n    udp_pool: dns\n    proxy_protocol: true\n").validate().expect("udp listener");
        parse("  - address: 0.0.0.0:5353\n    tcp_pool: dns\n    tls_mode: terminate\n    tls_host: x\n    proxy_protocol: true\n")
            .validate()
            .expect_err("tls_host x unknown, but proxy_protocol itself is fine on tcp listeners");
        let cfg = parse("  - address: 0.0.0.0:443\n    tls: true\n    proxy_protocol: true\n");
        assert!(err(&cfg).contains("not supported on tls listeners"));
    }

    #[test]
    fn udp_flow_timeout_zero_is_rejected() {
        let mut cfg = parse("  - address: 0.0.0.0:53\n    udp_pool: dns\n");
        cfg.keel.udp_flow_timeout_seconds = 0;
        assert!(err(&cfg).contains("udp_flow_timeout_seconds"));
    }
}
