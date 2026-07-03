use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

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

            // Vhosts and listeners are appended in load order.
            cfg.vhosts.extend(fragment.vhosts);
            cfg.listeners.extend(fragment.listeners);
        }
    }

    cfg.validate()?;
    Ok(cfg)
}

/// Reject conf.d files that contain root-only sections.
fn check_no_root_sections(path: &str, raw: &str) -> Result<()> {
    let value: serde_yml::Value = serde_yml::from_str(raw)
        .with_context(|| format!("cannot parse {path}"))?;

    let forbidden = ["keel", "metrics", "access_log", "include", "cluster", "acme"];
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
}

impl Config {
    fn validate(&self) -> Result<()> {
        for (name, pool) in &self.pools {
            if pool.backends.is_empty() {
                anyhow::bail!("pool '{name}' has no backends");
            }
        }
        for vhost in &self.vhosts {
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
                if tls.acme {
                    if tls.cert.is_some() || tls.key.is_some() {
                        anyhow::bail!(
                            "vhost '{}': tls.acme is true — remove cert/key (they are managed in the ACME storage dir)",
                            vhost.host
                        );
                    }
                    if vhost.host.contains('*') {
                        anyhow::bail!(
                            "vhost '{}': ACME HTTP-01 cannot issue wildcard certificates",
                            vhost.host
                        );
                    }
                } else if tls.cert.is_none() || tls.key.is_none() {
                    anyhow::bail!(
                        "vhost '{}': tls requires cert and key paths (or acme: true)",
                        vhost.host
                    );
                }
            }
        }
        if let Some(acme) = &self.acme {
            for domain in &acme.domains {
                if domain.contains('*') {
                    anyhow::bail!("acme.domains '{domain}': HTTP-01 cannot issue wildcard certificates");
                }
            }
        }
        Ok(())
    }

    /// The effective ACME config: the `acme:` block, or defaults when any vhost
    /// enables `tls.acme` without one. `None` when ACME is unused entirely.
    pub fn acme_effective(&self) -> Option<AcmeConfig> {
        let vhost_acme = self.vhosts.iter().any(|v| v.tls.as_ref().map_or(false, |t| t.acme));
        match (&self.acme, vhost_acme) {
            (Some(a), _) => Some(a.clone()),
            (None, true) => Some(AcmeConfig::default()),
            (None, false) => None,
        }
    }

    /// All hostnames Keel manages certificates for: ACME vhosts plus the
    /// standalone `acme.domains` list. Deduplicated, order preserved.
    pub fn acme_hosts(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut hosts = Vec::new();
        for v in &self.vhosts {
            if v.tls.as_ref().map_or(false, |t| t.acme) && seen.insert(v.host.clone()) {
                hosts.push(v.host.clone());
            }
        }
        if let Some(acme) = &self.acme {
            for d in &acme.domains {
                if seen.insert(d.clone()) {
                    hosts.push(d.clone());
                }
            }
        }
        hosts
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
}

impl Default for KeelConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            user: default_user(),
            group: default_group(),
            control_socket: default_control_socket(),
        }
    }
}

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

    #[serde(default)]
    pub proxy_protocol: bool,
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

    #[serde(default)]
    pub backends: Vec<Backend>,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LbAlgorithm {
    #[default]
    RoundRobin,
    Random,
    LeastConnections,
    ConsistentHash,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthCheck {
    #[serde(rename = "type")]
    pub kind: HealthCheckKind,

    #[serde(default = "default_health_path")]
    pub path: String,

    #[serde(default = "default_health_interval")]
    pub interval: String,

    #[serde(default = "default_health_timeout")]
    pub timeout: String,

    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,

    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckKind {
    Tcp,
    Http,
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
}

impl Vhost {
    /// Effective HTTP→HTTPS redirect: explicit value wins; ACME vhosts default
    /// to true (the challenge path bypasses the redirect in the proxy).
    pub fn redirect_http_effective(&self) -> bool {
        self.redirect_http
            .unwrap_or_else(|| self.tls.as_ref().map_or(false, |t| t.acme))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    pub path: String,
    pub pool: String,
    /// Per-route cache config. Overrides the vhost-level cache config when set.
    pub cache: Option<VhostCacheConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Certificate path (BYO cert). Not set when `acme: true`.
    pub cert: Option<String>,
    /// Private key path (BYO cert). Not set when `acme: true`.
    pub key: Option<String>,
    /// Obtain and renew the certificate automatically via ACME (HTTP-01).
    #[serde(default)]
    pub acme: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcmeConfig {
    /// Contact email for the ACME account (expiry warnings from the CA).
    pub email: Option<String>,

    /// ACME v2 directory URL. Defaults to Let's Encrypt production.
    #[serde(default = "default_acme_directory")]
    pub directory: String,

    /// Directory for certificates, keys, the account, and challenge tokens.
    #[serde(default = "default_acme_storage")]
    pub storage: String,

    /// Extra hostnames to obtain certificates for even though they have no TLS
    /// vhost — e.g. domains Keel fronts as plain TCP / TLS passthrough. Keel
    /// serves only the HTTP-01 challenge for these; the cert/key files land in
    /// `storage` for the operator or backend to consume (Lego standalone-style).
    #[serde(default)]
    pub domains: Vec<String>,

    /// PEM file with an additional trust root for the ACME directory itself.
    /// Only needed when testing against Pebble or an internal CA.
    pub root_ca: Option<String>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            email: None,
            directory: default_acme_directory(),
            storage: default_acme_storage(),
            domains: Vec::new(),
            root_ca: None,
        }
    }
}

fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_acme_storage() -> String { "/var/lib/keel/acme".into() }

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
