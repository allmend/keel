use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod fmt;
mod hcl;
pub mod validate;

/// Default location of the node-local file. A node without one runs on the
/// defaults of every node setting.
pub const DEFAULT_NODE_PATH: &str = "/etc/keel/node.hcl";

/// The replicated config: every file under the config directory by relative
/// path. Only `*.hcl` files are parsed as config; others (certificates)
/// travel with them unchanged.
pub use keel_control::FileSet;

/// The config directory's root file, loaded before every other `*.hcl`.
pub const ROOT_FILE: &str = "keel.hcl";

/// Load the node file at `node_path` and the config directory it names
/// (`keel.config_dir`). A missing file at the default path means defaults.
pub fn load(node_path: &str) -> Result<Config> {
    let node_file = read_node_file(node_path)?;
    let dir = config_dir(node_path, &node_file)?;
    let files = read_config_dir(&dir)?;
    let mut cfg = build(node_path, &node_file, &files, Some(&dir))?;
    cfg.path = node_path.to_owned();
    cfg.node_file = node_file;
    Ok(cfg)
}

/// The node file's text; empty when the default one does not exist.
pub fn read_node_file(node_path: &str) -> Result<String> {
    match fs::read_to_string(node_path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && node_path == DEFAULT_NODE_PATH => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {node_path}")),
    }
}

/// `node_file` without its `cluster` block: what the workers get. The join
/// secret stays with the control worker.
pub fn without_cluster_section(node_file: &str) -> String {
    let Ok(mut body) = hcl_edit::parser::parse_body(node_file) else { return String::new() };
    body.remove_blocks("cluster");
    body.to_string()
}

impl Config {
    /// This config as the workers get it: without the `cluster` block and
    /// its join secret.
    pub fn for_workers(&self) -> Config {
        let mut cfg = self.clone();
        cfg.cluster = None;
        cfg.node_file = without_cluster_section(&self.node_file);
        cfg
    }
}

/// `keel.config_dir` from a node file, or its default.
pub fn config_dir(node_path: &str, node_file: &str) -> Result<PathBuf> {
    let node = hcl::Source { name: node_path.to_owned(), text: node_file };
    let doc = hcl::read(Some(&node), &[])?;
    let dir = doc.value.get("keel").and_then(|k| k.get("config_dir")).and_then(|d| d.as_str());
    Ok(PathBuf::from(dir.map(str::to_owned).unwrap_or_else(default_config_dir)))
}

/// Every file below `dir`, by relative path.
pub fn read_config_dir(dir: &std::path::Path) -> Result<FileSet> {
    keel_control::read_file_set(dir)
}

/// The config files of a set in load order: the root file, then every other
/// `*.hcl` in path order. A YAML file is refused: the format changed.
pub fn config_files(files: &FileSet) -> Result<Vec<(&str, &str)>> {
    if let Some(old) = files.keys().find(|p| p.ends_with(".yaml") || p.ends_with(".yml")) {
        anyhow::bail!("{old}: the configuration is HCL now (*.hcl); YAML files are not read");
    }
    let root = files.get(ROOT_FILE).with_context(|| format!("the config directory has no {ROOT_FILE}"))?;
    let mut out = vec![(ROOT_FILE, root.as_str())];
    out.extend(files.iter().filter(|(p, _)| p.ends_with(".hcl") && p.as_str() != ROOT_FILE).map(|(p, t)| (p.as_str(), t.as_str())));
    Ok(out)
}

/// Build the config from this node's node file and a replicated file set.
pub fn assemble(node_file: &str, files: &FileSet) -> Result<Config> {
    build("node.hcl", node_file, files, None)
}

/// Build a config; errors name files below `dir` when given, else by their
/// path in the set.
fn build(node_name: &str, node_file: &str, files: &FileSet, dir: Option<&Path>) -> Result<Config> {
    let name = |p: &str| dir.map_or_else(|| p.to_owned(), |d| d.join(p).display().to_string());
    let listed = config_files(files).map_err(|e| match dir {
        Some(d) => anyhow::anyhow!("{}: {e:#}", d.display()),
        None => e,
    })?;
    let sources: Vec<hcl::Source> = listed.into_iter().map(|(p, text)| hcl::Source { name: name(p), text }).collect();
    let node = hcl::Source { name: node_name.to_owned(), text: node_file };
    let doc = hcl::read(Some(&node), &sources)?;
    let mut cfg: Config = from_document(&doc)?;
    cfg.files = files.clone();
    cfg.validate()?;
    crate::tls::check_files(&cfg)?;
    // The ACME client reads its extra trust root from disk: a relative
    // root_ca is the config directory's file there.
    let config_dir = std::path::PathBuf::from(&cfg.keel.config_dir);
    if let Some(acme) = cfg.acme.as_mut() {
        for issuer in acme.issuers.values_mut() {
            if let Some(key) = issuer.root_ca.as_deref().and_then(relative_key) {
                issuer.root_ca = Some(config_dir.join(key).to_string_lossy().into_owned());
            }
        }
    }
    Ok(cfg)
}

/// Deserialize a document; an error names the file, line and column of the
/// attribute or block it is about.
fn from_document<T: DeserializeOwned>(doc: &hcl::Document) -> Result<T> {
    serde_path_to_error::deserialize(serde_json::Value::Object(doc.value.clone())).map_err(|e| {
        let path = hcl::path_of(e.path());
        let mut msg = e.inner().to_string();
        let mut at = path.clone();
        for (serde, ours) in [("unknown field `", "unknown attribute `"), ("missing field `", "missing attribute `")] {
            if let Some(name) = msg.strip_prefix(serde).and_then(|rest| rest.split('`').next()) {
                if serde.starts_with("unknown") {
                    at = if path.is_empty() { name.to_owned() } else { format!("{path}.{name}") };
                }
                msg = format!("{ours}{name}`");
            }
        }
        match doc.locate(&at) {
            Some(pos) => hcl::error_at(pos, msg),
            None => anyhow::anyhow!("{msg}"),
        }
    })
}

/// A relative path in the config names a file of the config directory.
fn relative_key(path: &str) -> Option<&str> {
    (!std::path::Path::new(path).is_absolute()).then(|| path.trim_start_matches("./"))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The node file this config was loaded from — set by load(), not by the files.
    #[serde(skip, default)]
    pub path: String,

    /// The node file's text as loaded — set by load(). A cluster node builds
    /// every replicated version against it.
    #[serde(skip, default)]
    pub node_file: String,

    /// The config directory's files this config was built from — set by
    /// assemble(). Relative file paths in the config resolve against it.
    #[serde(skip, default)]
    pub files: FileSet,

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
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    pub remote: Option<RemoteControlConfig>,
}

// Unknown keys are refused: `ca_dir` is not a setting (the control CA lives
// in `keel.state_dir`), and a misspelt `allow` would silently open the port.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteControlConfig {
    /// TCP listen address for keelctl connections (mTLS required).
    pub address: String,

    /// Optional source-CIDR restriction, checked before the TLS handshake.
    /// Empty = allow any source. Narrows exposure; mTLS stays mandatory.
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.keel.control_user == self.keel.user {
            anyhow::bail!(
                "keel.control_user and keel.user are both '{}': the control worker holds the CA keys \
                 and must not share a user with the workers",
                self.keel.user
            );
        }
        if let Some(cluster) = &self.cluster {
            // Without it the join listener would hand a CA-signed identity to
            // any peer that reaches the cluster port.
            if cluster.secret.as_deref().unwrap_or("").is_empty() {
                anyhow::bail!("cluster.secret is required when node.hcl has a cluster block");
            }
        }
        // Keel resolves no names: every address it binds or connects to is
        // an IP and a port.
        let literal = |field: String, address: &str| -> Result<()> {
            if address.parse::<std::net::SocketAddr>().is_err() {
                anyhow::bail!("{field}: '{address}' is not an ip:port");
            }
            Ok(())
        };
        literal("metrics.address".into(), &self.metrics.address)?;
        if let Some(remote) = self.control.as_ref().and_then(|c| c.remote.as_ref()) {
            literal("control.remote.address".into(), &remote.address)?;
        }
        if let Some(cluster) = &self.cluster {
            literal("cluster.addr".into(), &cluster.addr)?;
            if let Some(advertise) = &cluster.advertise {
                literal("cluster.advertise".into(), advertise)?;
            }
            for member in &cluster.join {
                literal("cluster.join".into(), member)?;
            }
        }
        for l in &self.listeners {
            if l.addresses.is_empty() {
                anyhow::bail!("listener \"{}\": addresses lists none; a wildcard is [\"0.0.0.0\", \"::\"]", l.name);
            }
            if let Some(a) = l.addresses.iter().find(|a| a.parse::<std::net::IpAddr>().is_err()) {
                anyhow::bail!("listener \"{}\": '{a}' is not an IP", l.name);
            }
            if l.port == 0 {
                anyhow::bail!("listener \"{}\": port must be 1-65535", l.name);
            }
        }
        for (name, pool) in &self.pools {
            for b in &pool.backends {
                literal(format!("pool '{name}': backend"), &b.address)?;
            }
        }
        self.check_port_collisions()?;

        for (name, pool) in &self.pools {
            if pool.backends.is_empty() {
                anyhow::bail!("pool '{name}' has no backends");
            }
            if let Some(b) = pool.backends.iter().find(|b| !(1..=MAX_WEIGHT).contains(&b.weight)) {
                anyhow::bail!("pool '{name}': backend {} weight must be 1-{MAX_WEIGHT}, got {}", b.address, b.weight);
            }
            if pool.passive.failures == 0 {
                anyhow::bail!("pool '{name}': passive.failures must be at least 1");
            }
            if let Some(hc) = &pool.health_check {
                if hc.healthy_threshold == 0 || hc.unhealthy_threshold == 0 {
                    anyhow::bail!("pool '{name}': health_check thresholds must be at least 1");
                }
                if hc.port == Some(0) {
                    anyhow::bail!("pool '{name}': health_check.port must be 1-65535");
                }
                match &hc.probe {
                    ProbeConfig::Http { path, .. } if !path.starts_with('/') => {
                        anyhow::bail!("pool '{name}': health_check.path must start with '/'");
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
        crate::cache::parse_size(&self.keel.max_header_size).map_err(|e| anyhow::anyhow!("keel.max_header_size: {e}"))?;
        if self.keel.udp_max_flows == 0 {
            anyhow::bail!("keel.udp_max_flows must be at least 1");
        }
        let mut names = HashSet::new();
        // http, https and tcp listeners share TCP ports; udp has its own.
        let mut bound: Vec<(bool, std::net::SocketAddr, &str)> = Vec::new();
        for l in &self.listeners {
            let id = format!("listener \"{}\"", l.name);
            if l.name.is_empty() || !l.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                anyhow::bail!("{id}: a name is letters, digits, - and _");
            }
            if !names.insert(l.name.as_str()) {
                anyhow::bail!("{id} is defined twice");
            }
            let udp = l.protocol == Protocol::Udp;
            for e in l.endpoints() {
                if let Some((_, _, other)) = bound.iter().find(|(u, b, _)| *u == udp && overlaps(*b, e)) {
                    anyhow::bail!("{id}: {e} is also listener \"{other}\"'s");
                }
                bound.push((udp, e, l.name.as_str()));
            }
            match (l.protocol, &l.pool) {
                (Protocol::Tcp | Protocol::Udp, None) => anyhow::bail!("{id} needs a pool"),
                (Protocol::Tcp | Protocol::Udp, Some(pool)) if !self.pools.contains_key(pool) => {
                    anyhow::bail!("{id} references unknown pool '{pool}'")
                }
                (Protocol::Http | Protocol::Https, Some(_)) => {
                    anyhow::bail!("{id}: an HTTP listener routes by vhost and takes no pool")
                }
                _ => {}
            }
            if matches!(l.protocol, Protocol::Http | Protocol::Https) && !(l.allow.is_empty() && l.deny.is_empty()) {
                anyhow::bail!("{id}: allow and deny apply to tcp and udp listeners; HTTP takes them on the vhost or route");
            }
            crate::access::AccessList::new(&l.allow, &l.deny).map_err(|e| anyhow::anyhow!("{id}: {e}"))?;
            if l.protocol != Protocol::Tcp && l.idle_timeout.is_some() {
                anyhow::bail!("{id}: idle_timeout applies to tcp listeners; UDP has keel.udp_flow_timeout");
            }
            if l.protocol != Protocol::Tcp && (l.tls_mode != TlsMode::Passthrough || l.tls_host.is_some() || l.tls_verify || l.tls_ca.is_some()) {
                anyhow::bail!("{id}: tls_mode, tls_host, tls_verify and tls_ca apply to tcp listeners only");
            }
            if l.protocol == Protocol::Tcp {
                match l.tls_mode {
                    TlsMode::Passthrough => {
                        if l.tls_host.is_some() || l.tls_verify || l.tls_ca.is_some() {
                            anyhow::bail!("{id}: tls_host, tls_verify and tls_ca need tls_mode terminate or reencrypt");
                        }
                    }
                    TlsMode::Terminate | TlsMode::Reencrypt => {
                        let Some(host) = &l.tls_host else {
                            anyhow::bail!("{id}: tls_mode {:?} needs tls_host (a certificate block or a vhost with tls)", l.tls_mode);
                        };
                        let known = self.certificates.iter().any(|c| &c.host == host)
                            || self.vhosts.iter().any(|v| &v.host == host && v.tls.is_some());
                        if !known {
                            anyhow::bail!("{id}: tls_host '{host}' matches no certificate block or vhost with tls");
                        }
                        if l.tls_mode == TlsMode::Terminate && (l.tls_verify || l.tls_ca.is_some()) {
                            anyhow::bail!("{id}: tls_verify and tls_ca apply to tls_mode reencrypt only");
                        }
                        if l.tls_ca.is_some() && !l.tls_verify {
                            anyhow::bail!("{id}: tls_ca needs tls_verify = true");
                        }
                    }
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
            for cidr in vhost.forwarded_headers.iter().flat_map(|f| &f.trusted_proxies) {
                cidr.parse::<ipnet::IpNet>()
                    .map_err(|e| anyhow::anyhow!("vhost '{}': forwarded_headers.trusted_proxies: invalid CIDR '{cidr}': {e}", vhost.host))?;
            }
            crate::access::AccessList::new(&vhost.allow, &vhost.deny).map_err(|e| anyhow::anyhow!("vhost '{}': {e}", vhost.host))?;
            let sizes = std::iter::once((vhost.max_body_size.as_ref(), String::new()))
                .chain(vhost.routes.iter().map(|r| (r.max_body_size.as_ref(), format!(" route '{}'", r.path))));
            for (size, route) in sizes {
                if let Some(size) = size {
                    crate::cache::parse_size(size).map_err(|e| anyhow::anyhow!("vhost '{}'{route}: max_body_size: {e}", vhost.host))?;
                }
            }
            for r in &vhost.routes {
                crate::access::AccessList::new(&r.allow, &r.deny)
                    .map_err(|e| anyhow::anyhow!("vhost '{}': route '{}': {e}", vhost.host, r.path))?;
            }
            if vhost.pool.is_some() && !vhost.routes.is_empty() {
                anyhow::bail!("vhost '{}': pool and route blocks exclude each other; add route \"/\" instead", vhost.host);
            }
            if let Some(pool) = &vhost.pool {
                if !self.pools.contains_key(pool) {
                    anyhow::bail!("vhost '{}' references unknown pool '{pool}'", vhost.host);
                }
            }
            let mut paths = HashSet::new();
            for route in &vhost.routes {
                if !paths.insert(route.path.as_str()) {
                    anyhow::bail!("vhost '{}': route \"{}\" is defined twice", vhost.host, route.path);
                }
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
                    if vhost.host.contains('*') {
                        anyhow::bail!(
                            "vhost '{}': ACME cannot issue wildcard certificates",
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
        if let Some(memory) = &self.cache.memory {
            crate::cache::parse_size(memory).map_err(|e| anyhow::anyhow!("cache.memory: {e}"))?;
        }
        if let Some(disk) = &self.cache.disk {
            crate::cache::parse_size(&disk.size).map_err(|e| anyhow::anyhow!("cache.disk.size: {e}"))?;
        }
        self.validate_acme()?;
        self.validate_paths()?;
        Ok(())
    }

    /// Read a file the config names. A relative path is a file of the config
    /// directory, taken from the set this config was built from — so a pushed
    /// version brings its certificates with it, before any of it is written
    /// to disk. An absolute path is read from this node's disk.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        match relative_key(path) {
            Some(key) => self
                .files
                .get(key)
                .map(|text| text.as_bytes().to_vec())
                .with_context(|| format!("{path} is not in the config directory")),
            None => fs::read(path).with_context(|| format!("cannot read {path}")),
        }
    }

    /// Relative file paths must be files of the config directory, so a set
    /// that names a certificate it does not carry is refused before it is
    /// committed. Directories Keel writes to are node-local and absolute, and
    /// issued certificates never go into the config directory, which a push
    /// replaces.
    /// The control and peer ports are node listeners of their own; one that
    /// is also in `listeners:` would race for the port at bind time.
    fn check_port_collisions(&self) -> Result<()> {
        use std::net::SocketAddr;
        let own = [
            ("control.remote.address", self.control.as_ref().and_then(|c| c.remote.as_ref()).map(|r| r.address.as_str())),
            ("cluster.addr", self.cluster.as_ref().map(|c| c.addr.as_str())),
        ];
        for (field, address) in own {
            let Some(address) = address else { continue };
            let Ok(ours) = address.parse::<SocketAddr>() else { continue };
            for l in self.listeners.iter().filter(|l| l.protocol != Protocol::Udp) {
                if l.endpoints().into_iter().any(|theirs| overlaps(ours, theirs)) {
                    anyhow::bail!("{field} {address} collides with listener \"{}\"", l.name);
                }
            }
        }
        Ok(())
    }

    fn validate_paths(&self) -> Result<()> {
        let carried = |owner: String, field: &str, path: &Option<String>| -> Result<()> {
            match path.as_deref().and_then(relative_key) {
                Some(key) if !self.files.contains_key(key) => {
                    anyhow::bail!("{owner}: {field} '{key}' is not in the config directory")
                }
                _ => Ok(()),
            }
        };
        for v in &self.vhosts {
            if let Some(tls) = &v.tls {
                carried(format!("vhost '{}'", v.host), "tls.cert", &tls.cert)?;
                carried(format!("vhost '{}'", v.host), "tls.key", &tls.key)?;
            }
        }
        for c in &self.certificates {
            carried(format!("certificate '{}'", c.host), "cert", &c.cert)?;
            carried(format!("certificate '{}'", c.host), "key", &c.key)?;
        }
        for l in &self.listeners {
            carried(format!("listener \"{}\"", l.name), "tls_ca", &l.tls_ca)?;
        }
        if let Some(acme) = &self.acme {
            for (name, issuer) in &acme.issuers {
                carried(format!("acme issuer '{name}'"), "root_ca", &issuer.root_ca)?;
            }
        }

        let absolute = |field: &str, path: &str| -> Result<()> {
            if !std::path::Path::new(path).is_absolute() {
                anyhow::bail!("{field} must be an absolute path, got '{path}': Keel writes there, on each node");
            }
            Ok(())
        };
        if self.access_log.dir != "-" {
            absolute("access_log.dir", &self.access_log.dir)?;
        }
        if let Some(disk) = &self.cache.disk {
            absolute("cache.disk.path", &disk.path)?;
        }
        if let Some(acme) = &self.acme {
            absolute("acme.storage", &acme.storage)?;
            let config_dir = std::path::Path::new(&self.keel.config_dir);
            if std::path::Path::new(&acme.storage).starts_with(config_dir) {
                anyhow::bail!(
                    "acme.storage '{}' is inside the config directory ({}); a push replaces that directory, so issued certificates live elsewhere",
                    acme.storage,
                    config_dir.display()
                );
            }
        }
        Ok(())
    }

    fn validate_acme(&self) -> Result<()> {
        let issuer_defined = |name: &str| {
            self.acme.as_ref().is_some_and(|a| a.issuers.contains_key(name))
        };

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
                    "certificates '{}': ACME cannot issue wildcard certificates",
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
#[serde(deny_unknown_fields)]
pub struct KeelConfig {
    #[serde(default = "default_workers")]
    pub workers: usize,

    #[serde(default = "default_user")]
    pub user: String,

    #[serde(default = "default_group")]
    pub group: String,

    /// User the control worker runs as: Raft, the control listeners, ACME.
    /// Its primary group owns what only it may read (the state directory);
    /// it joins `group` to reach the workers and share the config directory.
    #[serde(default = "default_control_user")]
    pub control_user: String,

    #[serde(default = "default_control_socket")]
    pub control_socket: String,

    /// Time in-flight requests get to finish on graceful shutdown before
    /// the process exits. Keep below the supervisor's kill timeout
    /// (docker stop default 10s, K8s terminationGracePeriodSeconds 30s).
    #[serde(default = "default_grace_period", with = "duration")]
    pub grace_period: Duration,

    /// How long a UDP flow (one client ip:port on a `udp` listener) may stay
    /// idle before it expires and releases its backend.
    #[serde(default = "default_udp_flow_timeout", with = "duration")]
    pub udp_flow_timeout: Duration,

    /// Most UDP flows each `udp` listener will hold open at once (a
    /// worker runs one service per such listener). A flow costs an upstream
    /// socket and a task, so an unbounded table lets a datagram flood from
    /// spoofed sources exhaust the worker's descriptors — which its HTTP and
    /// TCP listeners share.
    #[serde(default = "default_udp_max_flows")]
    pub udp_max_flows: usize,

    /// How long workers a newer generation replaced keep serving their open
    /// connections before those are closed.
    #[serde(default = "default_drain_timeout", with = "duration")]
    pub drain_timeout: Duration,

    /// HTTP: how long a client connection is kept open between requests.
    #[serde(default = "default_keepalive_timeout", with = "duration")]
    pub keepalive_timeout: Duration,

    /// HTTP: longest wait between two reads of a request body.
    #[serde(default = "default_body_timeout", with = "duration")]
    pub body_timeout: Duration,

    /// HTTP: largest request line and headers, e.g. `64K`; larger is 431.
    #[serde(default = "default_max_header_size")]
    pub max_header_size: String,

    /// Directory for state Keel creates and keeps across restarts: the node
    /// ID today. Written by the unprivileged process after the drop.
    #[serde(default = "default_state_dir")]
    pub state_dir: String,

    /// The replicated config directory: `keel.hcl` and every other `*.hcl`
    /// below it. In cluster mode Keel writes it after each applied version.
    #[serde(default = "default_config_dir")]
    pub config_dir: String,
}

impl Default for KeelConfig {
    fn default() -> Self {
        Self {
            workers: default_workers(),
            user: default_user(),
            group: default_group(),
            control_user: default_control_user(),
            control_socket: default_control_socket(),
            grace_period: default_grace_period(),
            udp_flow_timeout: default_udp_flow_timeout(),
            udp_max_flows: default_udp_max_flows(),
            drain_timeout: default_drain_timeout(),
            keepalive_timeout: default_keepalive_timeout(),
            body_timeout: default_body_timeout(),
            max_header_size: default_max_header_size(),
            state_dir: default_state_dir(),
            config_dir: default_config_dir(),
        }
    }
}

fn default_state_dir() -> String { "/var/lib/keel".into() }

impl KeelConfig {
    /// Where the control CA lives: `ca.crt` / `ca.key` under the state directory.
    pub fn control_ca_dir(&self) -> String {
        format!("{}/control", self.state_dir.trim_end_matches('/'))
    }

    /// The directory holding the instance control socket; also the workers'
    /// sockets and the HTTP-01 challenge tokens, below it.
    pub fn runtime_dir(&self) -> PathBuf {
        Path::new(&self.control_socket).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/var/run/keel"))
    }
}
fn default_config_dir() -> String { "/etc/keel/config".into() }

fn default_grace_period() -> Duration { Duration::from_secs(10) }
fn default_drain_timeout() -> Duration { Duration::from_secs(3600) }
fn default_keepalive_timeout() -> Duration { Duration::from_secs(75) }
fn default_body_timeout() -> Duration { Duration::from_secs(60) }
fn default_max_header_size() -> String { "64K".into() }
fn default_udp_flow_timeout() -> Duration { Duration::from_secs(30) }
fn default_udp_max_flows() -> usize { 8192 }

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}
fn default_user() -> String { "keel".into() }
fn default_group() -> String { "keel".into() }
fn default_control_user() -> String { "keel-control".into() }
fn default_control_socket() -> String { "/var/run/keel/keel.sock".into() }

/// `listener "<name>" { ... }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// The label: the same on every node, for logs, metrics and status.
    pub name: String,

    pub protocol: Protocol,

    /// IPs to listen on. The same on every node; each node binds those it
    /// has.
    pub addresses: Vec<String>,

    pub port: u16,

    /// The pool a `tcp` or `udp` listener forwards to. HTTP listeners route
    /// by vhost instead.
    #[serde(default)]
    pub pool: Option<String>,

    /// Expect a PROXY protocol v1/v2 header on every connection (TCP, HTTP)
    /// or datagram (UDP) and take the client address from it. Connections
    /// without one are rejected. On `https` listeners the header is read
    /// before the handshake.
    #[serde(default)]
    pub proxy_protocol: bool,

    /// TLS handling on a `tcp` listener.
    #[serde(default)]
    pub tls_mode: TlsMode,

    /// Certificate served by a terminating listener: the host of a
    /// `certificate` block or of a vhost with `tls`. Used when the client
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

    /// Client networks let in; empty lets in every client not denied.
    #[serde(default)]
    pub allow: Vec<String>,

    /// Client networks turned away, before `allow` is looked at.
    #[serde(default)]
    pub deny: Vec<String>,

    /// `tcp`: a connection with no bytes either way for this long is
    /// closed. Default 1h.
    #[serde(default, with = "duration::option")]
    pub idle_timeout: Option<Duration>,
}

/// What a listener speaks: HTTP routed by vhost (plain or TLS-terminated),
/// or a TCP or UDP pool.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Http,
    Https,
    Tcp,
    Udp,
}

impl Listener {
    /// Every `ip:port` the listener names. Validated: the addresses are IPs.
    pub fn endpoints(&self) -> Vec<std::net::SocketAddr> {
        self.addresses.iter().filter_map(|a| a.parse().ok()).map(|ip| std::net::SocketAddr::new(ip, self.port)).collect()
    }

    /// The endpoints this node has, and those it has not.
    pub fn split_here(&self) -> (Vec<std::net::SocketAddr>, Vec<std::net::SocketAddr>) {
        self.endpoints().into_iter().partition(|e| on_this_node(e.ip()))
    }

    /// Terminates TLS for HTTP.
    pub fn tls(&self) -> bool {
        self.protocol == Protocol::Https
    }

    /// The pool of a `tcp` listener.
    pub fn tcp_pool(&self) -> Option<&str> {
        self.pool.as_deref().filter(|_| self.protocol == Protocol::Tcp)
    }

    /// The pool of a `udp` listener.
    pub fn udp_pool(&self) -> Option<&str> {
        self.pool.as_deref().filter(|_| self.protocol == Protocol::Udp)
    }
}

/// Two sockets on one port that cannot both be bound: the same IP, or a
/// wildcard and anything of the same family.
fn overlaps(a: std::net::SocketAddr, b: std::net::SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || (a.is_ipv4() == b.is_ipv4() && (a.ip().is_unspecified() || b.ip().is_unspecified())))
}

/// Whether this machine holds `ip`: a wildcard always, else what binding it
/// says. Only "not available" means no; any other error is the real bind's
/// to report.
pub fn on_this_node(ip: std::net::IpAddr) -> bool {
    ip.is_unspecified()
        || std::net::UdpSocket::bind((ip, 0)).map_or_else(|e| e.kind() != std::io::ErrorKind::AddrNotAvailable, |_| true)
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
#[serde(deny_unknown_fields)]
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
fn default_metrics_address() -> String { "127.0.0.1:10790".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    #[serde(default)]
    pub algorithm: LbAlgorithm,

    pub health_check: Option<HealthCheck>,

    /// Passive detection from real traffic; on by default.
    #[serde(default)]
    pub passive: PassiveConfig,

    /// Time to open a connection to a backend.
    #[serde(default = "default_connect_timeout", with = "duration")]
    pub connect_timeout: Duration,

    /// HTTP: longest wait for the backend between two reads.
    #[serde(default = "default_upstream_timeout", with = "duration")]
    pub read_timeout: Duration,

    /// HTTP: longest wait for the backend between two writes.
    #[serde(default = "default_upstream_timeout", with = "duration")]
    pub write_timeout: Duration,

    /// HTTP: how long an unused connection to a backend is kept for reuse.
    #[serde(default = "default_upstream_timeout", with = "duration")]
    pub idle_timeout: Duration,

    /// HTTP: a request whose connection failed tries one other backend.
    /// Never once bytes were sent.
    #[serde(default = "default_true")]
    pub retry: bool,

    #[serde(default)]
    pub backends: Vec<Backend>,
}

fn default_connect_timeout() -> Duration { Duration::from_secs(3) }
fn default_upstream_timeout() -> Duration { Duration::from_secs(60) }

/// Outlier ejection: a backend whose real requests, connections, or UDP
/// flows fail `failures` times in a row is taken out for `eject_for`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PassiveConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_passive_failures")]
    pub failures: u32,

    #[serde(default = "default_passive_eject_for", with = "duration")]
    pub eject_for: Duration,
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
fn default_passive_eject_for() -> Duration { Duration::from_secs(30) }

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

    #[serde(with = "duration")]
    pub interval: Duration,
    #[serde(with = "duration")]
    pub timeout: Duration,
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
    /// HTTP GET; 200–399 (or `expect_status`) and optionally `expect_body`.
    Http {
        #[serde(default = "default_health_path")]
        path: String,
        /// Host header and TLS SNI; defaults to the backend address.
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        tls: bool,
        /// Acceptable status codes; empty means 200–399.
        #[serde(default)]
        expect_status: Vec<StatusRange>,
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
    /// at least `min_valid`. The chain is not verified.
    Tls {
        /// SNI to send; default: the backend IP (no SNI).
        #[serde(default)]
        sni: Option<String>,
        #[serde(default, with = "duration::option")]
        min_valid: Option<Duration>,
    },
}

/// An inclusive range of HTTP status codes in `expect_status`: written as a
/// number (`204`), or a string of one code (`"204"`) or a range (`"200-299"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRange {
    pub lo: u16,
    pub hi: u16,
}

impl StatusRange {
    pub fn contains(&self, status: u16) -> bool {
        (self.lo..=self.hi).contains(&status)
    }

    fn parse(s: &str) -> Result<Self> {
        let wrong = || anyhow::anyhow!("expect_status entry \"{s}\" is not a status code or a range such as \"200-299\"");
        let (lo, hi) = match s.split_once('-') {
            Some((lo, hi)) => (lo.trim().parse().map_err(|_| wrong())?, hi.trim().parse().map_err(|_| wrong())?),
            None => {
                let code = s.trim().parse().map_err(|_| wrong())?;
                (code, code)
            }
        };
        Self::new(lo, hi).map_err(|e| anyhow::anyhow!("expect_status entry \"{s}\": {e}"))
    }

    fn new(lo: u16, hi: u16) -> Result<Self> {
        if !(100..=599).contains(&lo) || !(100..=599).contains(&hi) {
            anyhow::bail!("status codes are 100-599");
        }
        if lo > hi {
            anyhow::bail!("{lo} is above {hi}");
        }
        Ok(StatusRange { lo, hi })
    }
}

impl<'de> Deserialize<'de> for StatusRange {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(s) => StatusRange::parse(&s).map_err(D::Error::custom),
            serde_json::Value::Number(n) => {
                let code = n.as_u64().and_then(|c| u16::try_from(c).ok()).ok_or_else(|| D::Error::custom(format!("expect_status entry {n} is not a status code")))?;
                StatusRange::new(code, code).map_err(|e| D::Error::custom(format!("expect_status entry {n}: {e}")))
            }
            other => Err(D::Error::custom(format!("expect_status entry {other} is not a status code or a range"))),
        }
    }
}

impl Serialize for StatusRange {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if self.lo == self.hi {
            s.serialize_u16(self.lo)
        } else {
            s.serialize_str(&format!("{}-{}", self.lo, self.hi))
        }
    }
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
        "tls" => &["sni", "min_valid"],
        _ => return None,
    })
}

/// Shape serde parses; `HealthCheck` wraps it after the strict key check.
#[derive(Deserialize)]
struct RawHealthCheck {
    #[serde(flatten)]
    probe: ProbeConfig,
    #[serde(default = "default_health_interval", with = "duration")]
    interval: Duration,
    #[serde(default = "default_health_timeout", with = "duration")]
    timeout: Duration,
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
        let value = serde_json::Value::deserialize(d)?;
        let map = value
            .as_object()
            .ok_or_else(|| D::Error::custom("health_check must be a block"))?;
        let kind = map
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| D::Error::custom("health_check needs its type as label"))?
            .to_owned();
        let extra = probe_fields(&kind).ok_or_else(|| {
            D::Error::custom(format!(
                "health_check.type '{kind}' is not one of: {}",
                PROBE_TYPES.join(", ")
            ))
        })?;
        for k in map.keys().map(String::as_str) {
            if !HEALTH_COMMON_FIELDS.contains(&k) && !extra.contains(&k) {
                return Err(D::Error::custom(format!(
                    "health_check: attribute '{k}' is not valid for type '{kind}'"
                )));
            }
        }
        let raw: RawHealthCheck = serde_json::from_value(value).map_err(D::Error::custom)?;
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

/// Units of a duration in the config, in seconds. A second is the smallest.
const DURATION_UNITS: [(&str, u64); 6] =
    [("s", 1), ("m", 60), ("h", 3600), ("d", 86_400), ("w", 7 * 86_400), ("y", 365 * 86_400)];

/// A duration in the config: a whole number and a unit, `30s`, `5m`, `12h`,
/// `30d`, `2w` or `1y`; at least one second.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let wrong = || anyhow::anyhow!("'{s}' is not a duration: a whole number and one of s, m, h, d, w, y (e.g. 30s)");
    let n: u64 = num.parse().map_err(|_| wrong())?;
    let (_, scale) = DURATION_UNITS.iter().find(|(u, _)| *u == unit).ok_or_else(wrong)?;
    if n == 0 {
        anyhow::bail!("'{s}': a duration is at least 1s");
    }
    Ok(Duration::from_secs(n.saturating_mul(*scale)))
}

/// Attributes that hold a duration; `keel config validate --write` adds the
/// unit to a bare number there.
pub const DURATION_ATTRIBUTES: [&str; 14] = [
    "grace_period", "udp_flow_timeout", "eject_for", "interval", "timeout", "ttl", "min_valid", "connect_timeout",
    "read_timeout", "write_timeout", "idle_timeout", "keepalive_timeout", "body_timeout", "drain_timeout",
];

/// Serde for duration attributes: a string such as `30s`.
mod duration {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(s) => super::parse_duration(&s).map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!("expected a duration such as \"30s\", got {other}"))),
        }
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
            match d {
                Some(d) => super::serialize(d, s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
            super::deserialize(d).map(Some)
        }
    }
}

fn default_health_path() -> String { "/health".into() }
fn default_health_interval() -> Duration { Duration::from_secs(10) }
fn default_health_timeout() -> Duration { Duration::from_secs(2) }
fn default_healthy_threshold() -> u32 { 2 }
fn default_unhealthy_threshold() -> u32 { 3 }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub address: String,

    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 { 1 }

/// Upper bound for `backends[].weight`: consistent hashing places about 160
/// points per unit of weight on its ring.
const MAX_WEIGHT: u32 = 1000;

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
#[serde(deny_unknown_fields)]
pub struct ForwardedHeadersConfig {
    #[serde(default)]
    pub mode: ForwardedMode,
    /// CIDR ranges of trusted upstream proxies (used in Append mode).
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Memory budget, e.g. "256M", "1G". Memory cache disabled if absent.
    pub memory: Option<String>,
    /// Disk cache config. Disk cache disabled if absent.
    pub disk: Option<DiskCacheConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiskCacheConfig {
    /// Directory for cache files.
    pub path: String,
    /// Disk budget, e.g. "500M", "10G".
    pub size: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct VhostCacheConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Fallback TTL when origin sends no Cache-Control.
    #[serde(default, with = "duration::option")]
    pub ttl: Option<Duration>,
    /// HTTP status codes to cache. Defaults to [200] when empty.
    #[serde(default)]
    pub statuses: Vec<u16>,
    /// Content-type prefixes to cache. Empty means no restriction.
    /// Supports trailing wildcard: "image/*" matches "image/png", "image/jpeg", etc.
    #[serde(default)]
    pub content_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    /// Client networks let in; empty lets in every client not denied.
    #[serde(default)]
    pub allow: Vec<String>,

    /// Client networks turned away, before `allow` is looked at.
    #[serde(default)]
    pub deny: Vec<String>,

    /// Largest request body, e.g. `10M`; larger is 413. Unset: no limit.
    pub max_body_size: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
            .unwrap_or_else(|| self.tls.as_ref().is_some_and(|t| t.acme.enabled()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    pub pool: String,
    /// Per-route cache config. Overrides the vhost-level cache config when set.
    pub cache: Option<VhostCacheConfig>,

    /// Client networks let in; empty lets in every client not denied.
    #[serde(default)]
    pub allow: Vec<String>,

    /// Client networks turned away, before `allow` is looked at.
    #[serde(default)]
    pub deny: Vec<String>,

    /// Largest request body, e.g. `10M`; larger is 413. Unset: no limit.
    pub max_body_size: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Directory for certificates, keys, accounts, and challenge tokens.
    #[serde(default = "default_acme_storage")]
    pub storage: String,

    /// When to renew: a percentage of the certificate's total lifetime
    /// remaining ("30%", works for both 90-day and short-lived certs) or a
    /// duration ("20d"). Per-issuer override available.
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
}

impl Default for AcmeIssuer {
    fn default() -> Self {
        Self {
            email: None,
            directory: default_acme_directory(),
            root_ca: None,
            renew_before: None,
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
#[serde(deny_unknown_fields)]
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
    /// Renew when less than this time remains.
    Before(Duration),
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
        } else {
            parse_duration(s)
                .map(RenewBefore::Before)
                .map_err(|_| anyhow::anyhow!("renew_before must be a percentage ('30%') or a duration ('20d'), got '{s}'"))
        }
    }
}

fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_acme_storage() -> String { "/var/lib/keel/acme".into() }
fn default_renew_before() -> String { "30%".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

// Unknown keys are refused here: `node_id` is not a setting (the node ID is
// generated on first start), and a misspelt `advertise` would otherwise leave
// the node announcing its bind address.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Peer RPC listen address. Default: 0.0.0.0:7654
    #[serde(default = "default_cluster_addr")]
    pub addr: String,

    /// Address other nodes use to reach this one. Defaults to `addr`; required
    /// when `addr` is unspecified (0.0.0.0 or ::).
    pub advertise: Option<String>,

    pub secret: Option<String>,

    /// Members to join through on first start. Without it, a node with no
    /// stored state starts a cluster of its own.
    #[serde(default)]
    pub join: Vec<String>,
}

fn default_cluster_addr() -> String { "0.0.0.0:7654".into() }

/// A config from the text of one `keel.hcl`, not validated: for tests that
/// need a config of a certain shape.
#[cfg(test)]
pub fn parse(text: &str) -> Result<Config> {
    parse_with_node("", text)
}

/// A config from a node file and the text of one `keel.hcl`, not validated.
#[cfg(test)]
pub fn parse_with_node(node: &str, text: &str) -> Result<Config> {
    let node = hcl::Source { name: "node.hcl".into(), text: node };
    let doc = hcl::read(Some(&node), &[hcl::Source { name: ROOT_FILE.into(), text }])?;
    from_document(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "pool \"dns\" {\n  backend \"10.0.0.1:53\" {}\n}\n";

    fn with_pool(text: &str) -> Config {
        parse(&format!("{POOL}{text}")).expect("config parses")
    }

    fn err(cfg: &Config) -> String {
        cfg.validate().expect_err("validation should fail").to_string()
    }

    fn parse_err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should not parse"))
    }

    fn files(entries: &[(&str, &str)]) -> FileSet {
        entries.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
    }

    const ROOT: &str = r#"keel {
  grace_period = "3s"
}

listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "web" {
  backend "10.0.0.1:80" {}
}

vhost "a.test" {
  pool = "web"
}
"#;

    fn assemble_err(node: &str, set: &FileSet) -> String {
        format!("{:#}", assemble(node, set).expect_err("must be refused"))
    }

    #[test]
    fn node_file_and_config_directory_assemble_into_one_config() {
        let node = "keel {\n  workers    = 3\n  config_dir = \"/srv/lb\"\n}\nmetrics {\n  address = \"127.0.0.1:9999\"\n}\n";
        let set = files(&[
            ("keel.hcl", ROOT),
            ("vhosts/b.hcl", "vhost \"b.test\" {\n  pool = \"api\"\n}\n"),
            ("pools/api.hcl", "pool \"api\" {\n  backend \"10.0.0.2:80\" {}\n}\n"),
            ("certs/b.crt", "-----BEGIN CERTIFICATE-----\nnot parsed\n"),
        ]);
        let cfg = assemble(node, &set).unwrap();
        assert_eq!((cfg.keel.workers, cfg.keel.grace_period), (3, Duration::from_secs(3)), "keel from both files");
        assert_eq!(cfg.keel.config_dir, "/srv/lb");
        assert_eq!(cfg.metrics.address, "127.0.0.1:9999");
        assert!(cfg.pools.contains_key("web") && cfg.pools.contains_key("api"));
        let hosts: Vec<&str> = cfg.vhosts.iter().map(|v| v.host.as_str()).collect();
        assert_eq!(hosts, vec!["a.test", "b.test"], "root first, then the others in path order");
    }

    #[test]
    fn errors_name_file_line_and_column() {
        let set = files(&[("keel.hcl", ROOT), ("pools/api.hcl", "pool \"api\" {\n  backend \"10.0.0.2:80\" {}\n\n    algorithmn = \"round_robin\"\n}\n")]);
        assert_eq!(assemble_err("", &set), "pools/api.hcl:4:5: unknown attribute `algorithmn`");
        let set = files(&[("keel.hcl", &ROOT.replace("\"3s\"", "3"))]);
        let e = assemble_err("", &set);
        assert!(e.starts_with("keel.hcl:2:3: expected a duration such as \"30s\", got 3"), "{e}");
        let set = files(&[("keel.hcl", &ROOT.replace("pool = \"web\"", "pool = 7"))]);
        let e = assemble_err("", &set);
        assert!(e.starts_with("keel.hcl:16:3: invalid type: integer `7`, expected a string"), "{e}");
        let e = assemble_err("keel {\n  workers = \"many\"\n}\n", &files(&[("keel.hcl", ROOT)]));
        assert!(e.starts_with("node.hcl:2:3: invalid type: string \"many\""), "{e}");
        let set = files(&[("keel.hcl", &ROOT.replace("vhost \"a.test\" {", "vhost \"a.test\" {\n  tls {}"))]);
        assert_eq!(assemble_err("", &set), "vhost 'a.test': tls requires cert and key paths (or acme)");
    }

    #[test]
    fn yaml_files_and_a_missing_root_file_are_refused() {
        let e = assemble_err("", &files(&[("keel.hcl", ROOT), ("pools/old.yaml", "pools: {}\n")]));
        assert_eq!(e, "pools/old.yaml: the configuration is HCL now (*.hcl); YAML files are not read");
        let e = assemble_err("", &files(&[("pools.hcl", "")]));
        assert!(e.contains("has no keel.hcl"), "{e}");
    }

    #[test]
    fn replicated_sections_are_refused_in_the_node_file() {
        let set = files(&[("keel.hcl", ROOT)]);
        let e = assemble_err("listener \"http-81\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 81\n}\n", &set);
        assert!(e.contains("`listener` belongs in the config directory"), "{e}");
        let e = assemble_err("keel {\n  grace_period = \"5s\"\n}\n", &set);
        assert!(e.contains("`keel.grace_period` belongs in the config directory"), "{e}");
    }

    #[test]
    fn node_sections_are_refused_in_the_config_directory() {
        let e = assemble_err("", &files(&[("keel.hcl", &format!("{ROOT}cluster {{\n  addr = \"10.0.0.1:7654\"\n}}\n"))]));
        assert!(e.contains("keel.hcl:18:1: `cluster` belongs in the node file"), "{e}");
        let e = assemble_err("", &files(&[("keel.hcl", &ROOT.replace("grace_period = \"3s\"", "workers = 2"))]));
        assert!(e.contains("`keel.workers` belongs in the node file"), "{e}");
    }

    #[test]
    fn once_only_sections_stay_in_the_root_file() {
        let e = assemble_err("", &files(&[("keel.hcl", ROOT), ("more.hcl", "cache {\n  memory = \"1M\"\n}\n")]));
        assert!(e.contains("more.hcl:1:1: `cache` may only be set in keel.hcl"), "{e}");
    }

    #[test]
    fn the_config_directory_is_read_with_relative_paths() {
        let dir = std::env::temp_dir().join(format!("keel-configdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vhosts")).unwrap();
        std::fs::write(dir.join("keel.hcl"), ROOT).unwrap();
        std::fs::write(dir.join("vhosts/b.hcl"), "").unwrap();
        std::fs::write(dir.join(".keel.hcl.swp"), "editor").unwrap();
        let set = read_config_dir(&dir).unwrap();
        assert_eq!(set.keys().map(String::as_str).collect::<Vec<_>>(), vec!["keel.hcl", "vhosts/b.hcl"]);
        let node = std::env::temp_dir().join(format!("keel-node-{}.hcl", std::process::id()));
        std::fs::write(&node, format!("locals {{\n  dir = \"{}\"\n}}\nkeel {{\n  config_dir = local.dir\n}}\n", dir.display())).unwrap();
        let cfg = load(node.to_str().unwrap()).unwrap();
        assert_eq!(cfg.vhosts.len(), 1);
        std::fs::write(dir.join("vhosts/b.hcl"), "vhost \"b.test\" {\n  pool = \"nope\"\n}\n").unwrap();
        std::fs::write(dir.join("vhosts/c.hcl"), "vhost \"c.test\" {\n  pool = 1\n}\n").unwrap();
        let e = format!("{:#}", load(node.to_str().unwrap()).unwrap_err());
        assert_eq!(e, format!("{}/vhosts/c.hcl:2:3: invalid type: integer `1`, expected a string", dir.display()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&node);
    }

    const TLS_ROOT: &str = r#"listener "https" {
      protocol  = "https"
      addresses = ["0.0.0.0"]
      port      = 443
    }

pool "web" {
  backend "10.0.0.1:80" {}
}

vhost "a.test" {
  pool = "web"
  tls {
    cert = "certs/a.crt"
    key  = "./certs/a.key"
  }
}
"#;

    #[test]
    fn relative_file_paths_come_from_the_config_directory() {
        let pair = rcgen::generate_simple_self_signed(vec!["a.test".to_owned()]).unwrap();
        let (cert, key) = (pair.cert.pem(), pair.signing_key.serialize_pem());
        let set = files(&[("keel.hcl", TLS_ROOT), ("certs/a.crt", &cert), ("certs/a.key", &key)]);
        let cfg = assemble("", &set).unwrap();
        assert!(cfg.listeners[0].tls());
        assert_eq!(cfg.read_file("certs/a.crt").unwrap(), cert.as_bytes());
        assert_eq!(cfg.read_file("./certs/a.key").unwrap(), key.as_bytes());
        let e = assemble_err("", &files(&[("keel.hcl", TLS_ROOT), ("certs/a.crt", "CERT"), ("certs/a.key", &key)]));
        assert!(e.starts_with("vhost 'a.test': tls"), "{e}");
        let abs = std::env::temp_dir().join(format!("keel-abs-{}.crt", std::process::id()));
        std::fs::write(&abs, "DISK").unwrap();
        assert_eq!(cfg.read_file(abs.to_str().unwrap()).unwrap(), b"DISK", "absolute paths read from disk");
        let _ = std::fs::remove_file(&abs);
    }

    const WEB: &str = "pool \"web\" {\n  backend \"10.0.0.1:80\" {}\n}\n";

    #[test]
    fn the_control_worker_needs_a_user_of_its_own() {
        let e = assemble_err("keel {\n  user         = \"keel\"\n  control_user = \"keel\"\n}\n", &files(&[("keel.hcl", WEB)]));
        assert!(e.contains("keel.control_user and keel.user are both 'keel'"), "{e}");
    }

    #[test]
    fn a_cluster_section_needs_a_secret() {
        let e = assemble_err("cluster {\n  advertise = \"10.0.0.1:7654\"\n}\n", &files(&[("keel.hcl", WEB)]));
        assert!(e.contains("cluster.secret is required"), "{e}");
    }

    #[test]
    fn control_and_peer_ports_must_not_be_listeners() {
        let listening = |address: &str| {
            let (ip, port) = address.rsplit_once(':').unwrap();
            format!("{WEB}listener \"http\" {{\n  protocol  = \"http\"\n  addresses = [\"{ip}\"]\n  port      = {port}\n}}\n")
        };
        let remote = "control {\n  remote {\n    address = \"0.0.0.0:10789\"\n  }\n}\n";
        let e = assemble_err(remote, &files(&[("keel.hcl", &listening("10.0.0.1:10789"))]));
        assert!(e.contains("control.remote.address 0.0.0.0:10789 collides with listener \"http\""), "{e}");
        let peer = "cluster {\n  addr   = \"10.0.0.1:7654\"\n  secret = \"s\"\n}\n";
        let e = assemble_err(peer, &files(&[("keel.hcl", &listening("0.0.0.0:7654"))]));
        assert!(e.contains("cluster.addr 10.0.0.1:7654 collides"), "{e}");
        // Another address on the same port, or a UDP listener, is no collision.
        assemble(peer, &files(&[("keel.hcl", &listening("10.0.0.2:7654"))])).unwrap();
        let udp = format!("{WEB}listener \"web\" {{\n  protocol  = \"udp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 7654\n  pool      = \"web\"\n}}\n");
        assemble(peer, &files(&[("keel.hcl", &udp)])).unwrap();
    }

    #[test]
    fn workers_get_the_node_file_without_its_cluster_block() {
        let node = "keel {\n  workers = 2\n}\n\ncluster {\n  secret = \"s3cret\"\n}\n";
        let stripped = without_cluster_section(node);
        assert!(!stripped.contains("s3cret") && stripped.contains("workers = 2"), "{stripped}");
        assert_eq!(without_cluster_section("cluster {\n  secret = \"s\"\n}\n").trim(), "");
        let cfg = assemble(node, &files(&[("keel.hcl", WEB)])).unwrap();
        let mut loaded = cfg.clone();
        loaded.node_file = node.into();
        assert!(loaded.for_workers().cluster.is_none());
        assert!(!loaded.for_workers().node_file.contains("s3cret"));
        assemble(&loaded.for_workers().node_file, &files(&[("keel.hcl", WEB)])).expect("what workers get still loads");
    }

    #[test]
    fn a_relative_file_the_set_does_not_carry_is_refused() {
        let e = assemble_err("", &files(&[("keel.hcl", TLS_ROOT), ("certs/a.crt", "CERT")]));
        assert!(e.contains("vhost 'a.test': tls.key 'certs/a.key' is not in the config directory"), "{e}");
    }

    #[test]
    fn directories_keel_writes_to_are_absolute_and_outside_the_config_directory() {
        let e = assemble_err("", &files(&[("keel.hcl", &format!("{ROOT}access_log {{\n  dir = \"logs\"\n}}\n"))]));
        assert!(e.contains("access_log.dir must be an absolute path"), "{e}");
        let e = assemble_err("", &files(&[("keel.hcl", &format!("{ROOT}cache {{\n  disk {{\n    path = \"cache\"\n    size = \"1M\"\n  }}\n}}\n"))]));
        assert!(e.contains("cache.disk.path must be an absolute path"), "{e}");
        let acme = |storage: &str| format!("{ROOT}acme {{\n  storage = \"{storage}\"\n  issuer \"default\" {{\n    email = \"a@b.test\"\n  }}\n}}\n");
        let e = assemble_err("", &files(&[("keel.hcl", &acme("/etc/keel/config/acme"))]));
        assert!(e.contains("inside the config directory"), "{e}");
        let e = assemble_err("", &files(&[("keel.hcl", &acme("acme"))]));
        assert!(e.contains("acme.storage must be an absolute path"), "{e}");
    }

    #[test]
    fn a_relative_acme_root_ca_is_the_config_directory_file() {
        let root = format!("{ROOT}acme {{\n  issuer \"test\" {{\n    directory = \"https://ca.test/dir\"\n    root_ca   = \"certs/ca.pem\"\n  }}\n}}\n");
        let e = assemble_err("", &files(&[("keel.hcl", &root)]));
        assert!(e.contains("acme issuer 'test': root_ca 'certs/ca.pem' is not in the config directory"), "{e}");
        let cfg = assemble("keel {\n  config_dir = \"/srv/lb\"\n}\n", &files(&[("keel.hcl", &root), ("certs/ca.pem", "PEM")])).unwrap();
        assert_eq!(cfg.acme.unwrap().issuers["test"].root_ca.as_deref(), Some("/srv/lb/certs/ca.pem"));
    }

    #[test]
    fn the_shipped_examples_load_and_are_formatted() {
        for (node, root) in [
            (include_str!("../../node.hcl"), include_str!("../../keel.hcl")),
            (include_str!("../../dev/node.hcl"), include_str!("../../dev/config/keel.hcl")),
        ] {
            assemble(node, &files(&[("keel.hcl", root)])).expect("example loads");
            assert_eq!(fmt::format(node), node);
            assert_eq!(fmt::format(root), root);
        }
    }

    fn node_err(node: &str) -> String {
        format!("{:#}", parse_with_node(node, POOL).expect_err("should not parse"))
    }

    #[test]
    fn removed_and_misspelt_settings_are_refused() {
        let e = node_err("control {\n  remote {\n    address = \"127.0.0.1:10789\"\n    ca_dir  = \"/x\"\n  }\n}\n");
        assert!(e.contains("node.hcl:4:5: unknown attribute `ca_dir`"), "{e}");
        assert_eq!(KeelConfig::default().control_ca_dir(), "/var/lib/keel/control");
        for key in ["ca_cert", "ca_key", "node_id"] {
            let e = node_err(&format!("cluster {{\n  addr = \"10.0.0.1:7654\"\n  {key} = \"x\"\n}}\n"));
            assert!(e.contains(&format!("unknown attribute `{key}`")), "{e}");
        }
        let e = parse_err(&format!("{POOL}vhost \"a\" {{\n  pool    = \"dns\"\n  redirect_htp = true\n}}\n"));
        assert!(e.contains("unknown attribute `redirect_htp`"), "{e}");
    }

    #[test]
    fn defaults() {
        let cfg = parse_with_node("cluster {\n  addr      = \"0.0.0.0:7654\"\n  advertise = \"10.0.0.1:7654\"\n}\n", POOL).unwrap();
        assert_eq!(cfg.keel.state_dir, "/var/lib/keel");
        assert_eq!(cfg.cluster.unwrap().advertise.as_deref(), Some("10.0.0.1:7654"));
        // 9090 is Prometheus's own default, so a node colocated with a
        // Prometheus server collided with it; 10790 sits next to the remote
        // control port (10789) and outside the 9100-9999 exporter range.
        let cfg = with_pool("");
        assert_eq!(cfg.metrics.address, "127.0.0.1:10790");
        assert_eq!(cfg.keel.udp_flow_timeout, Duration::from_secs(30));
        assert_eq!(cfg.keel.grace_period, Duration::from_secs(10));
        let p = &cfg.pools["dns"].passive;
        assert!(p.enabled);
        assert_eq!((p.failures, p.eject_for), (5, Duration::from_secs(30)));
        cfg.validate().expect("valid");
    }

    #[test]
    fn addresses_are_ip_and_port() {
        let node = |text: &str| parse_with_node(text, POOL).unwrap();
        let remote = |address: &str| node(&format!("control {{\n  remote {{\n    address = \"{address}\"\n  }}\n}}\n"));
        assert_eq!(err(&remote("localhost:10789")), "control.remote.address: 'localhost:10789' is not an ip:port");
        remote("0.0.0.0:10789").validate().unwrap();
        let metrics = node("metrics {\n  address = \"metrics.internal:10790\"\n}\n");
        assert_eq!(err(&metrics), "metrics.address: 'metrics.internal:10790' is not an ip:port");
        let cluster = |field: &str| node(&format!("cluster {{\n  secret = \"s\"\n  {field}\n}}\n"));
        assert!(err(&cluster("addr = \"node1:7654\"")).starts_with("cluster.addr: 'node1:7654'"));
        assert!(err(&cluster("advertise = \"node1:7654\"")).starts_with("cluster.advertise: 'node1:7654'"));
        assert!(err(&cluster("join = [\"node1:7654\"]")).starts_with("cluster.join: 'node1:7654'"));
        cluster("advertise = \"[2001:db8::1]:7654\"").validate().unwrap();

        let e = |text: &str| err(&parse(text).unwrap());
        assert_eq!(e("pool \"web\" {\n  backend \"web1:80\" {}\n}\n"), "pool 'web': backend: 'web1:80' is not an ip:port");
        assert_eq!(e("pool \"web\" {\n  backend \"10.0.0.1\" {}\n}\n"), "pool 'web': backend: '10.0.0.1' is not an ip:port");
        assert_eq!(e(&format!("{POOL}listener \"web\" {{\n  protocol  = \"http\"\n  addresses = [\"localhost\"]\n  port      = 80\n}}\n")), "listener \"web\": 'localhost' is not an IP");
        assert!(e(&format!("{POOL}listener \"web\" {{\n  protocol  = \"http\"\n  addresses = []\n  port      = 80\n}}\n")).contains("addresses lists none"));
        parse("pool \"web\" {\n  backend \"[2001:db8::1]:8080\" {}\n}\nlistener \"web\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\", \"::\"]\n  port      = 80\n}\n").unwrap().validate().unwrap();
    }

    #[test]
    fn access_lists_are_networks_on_vhosts_routes_and_l4_listeners() {
        let e = |text: &str| err(&with_pool(text));
        assert_eq!(e("vhost \"a\" {\n  pool  = \"dns\"\n  allow = [\"nope\"]\n}\n"), "vhost 'a': 'nope' is not an IP network or address");
        assert_eq!(
            e("vhost \"a\" {\n  route \"/x/\" {\n    pool = \"dns\"\n    deny = [\"10.0.0.0/33\"]\n  }\n}\n"),
            "vhost 'a': route '/x/': '10.0.0.0/33' is not an IP network or address"
        );
        assert!(e("listener \"http-80\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n  allow     = [\"10.0.0.0/8\"]\n}\n").contains("allow and deny apply to tcp and udp listeners"));
        with_pool("listener \"dns\" {\n  protocol  = \"udp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 53\n  pool      = \"dns\"\n  allow     = [\"10.0.0.0/8\", \"2001:db8::1\"]\n}\n").validate().unwrap();
    }

    #[test]
    fn limits_and_timeouts_are_checked() {
        let e = |text: &str| err(&with_pool(text));
        assert!(e("keel {\n  max_header_size = \"lots\"\n}\n").starts_with("keel.max_header_size:"));
        assert!(e("vhost \"a\" {\n  pool          = \"dns\"\n  max_body_size = \"x\"\n}\n").starts_with("vhost 'a': max_body_size:"));
        assert!(e("listener \"http-80\" {\n  protocol     = \"http\"\n  addresses    = [\"0.0.0.0\"]\n  port         = 80\n  idle_timeout = \"5s\"\n}\n").contains("idle_timeout applies to tcp listeners"));
        let cfg = with_pool("listener \"dns-53\" {\n  protocol     = \"tcp\"\n  addresses    = [\"0.0.0.0\"]\n  port         = 53\n  pool         = \"dns\"\n  idle_timeout = \"5m\"\n}\n");
        cfg.validate().unwrap();
        assert_eq!(cfg.listeners[0].idle_timeout, Some(Duration::from_secs(300)));
        let p = &cfg.pools["dns"];
        assert_eq!((p.connect_timeout, p.read_timeout, p.retry), (Duration::from_secs(3), Duration::from_secs(60), true));
    }

    #[test]
    fn a_node_binds_the_addresses_it_has() {
        let cfg = with_pool("listener \"web\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\", \"127.0.0.1\", \"192.0.2.1\"]\n  port      = 80\n}\n");
        let (here, elsewhere) = cfg.listeners[0].split_here();
        let text = |v: Vec<std::net::SocketAddr>| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(text(here), ["0.0.0.0:80", "127.0.0.1:80"]);
        assert_eq!(text(elsewhere), ["192.0.2.1:80"], "a documentation address is on no machine");
    }

    #[test]
    fn listeners_by_protocol() {
        let cfg = with_pool("listener \"dns-udp\" {\n  protocol  = \"udp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 53\n  pool      = \"dns\"\n}\nlistener \"dns-tcp\" {\n  protocol  = \"tcp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 53\n  pool      = \"dns\"\n}\n");
        cfg.validate().expect("udp and tcp share an address and a pool");
        assert_eq!((cfg.listeners[0].udp_pool(), cfg.listeners[0].tcp_pool()), (Some("dns"), None));
        assert_eq!((cfg.listeners[1].udp_pool(), cfg.listeners[1].tcp_pool()), (None, Some("dns")));

        let e = |text: &str| err(&with_pool(text));
        assert!(e("listener \"resolvers\" {\n  protocol  = \"udp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 53\n  pool      = \"resolvers\"\n}\n").contains("references unknown pool 'resolvers'"));
        assert!(e("listener \"tcp\" {\n  protocol  = \"tcp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 53\n}\n").contains("needs a pool"));
        assert!(e("listener \"http-80\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n  pool      = \"dns\"\n}\n").contains("takes no pool"));
        assert!(e("listener \"http-80\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n}\nlistener \"dns-80\" {\n  protocol  = \"tcp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n  pool      = \"dns\"\n}\n").contains("listener \"dns-80\": 0.0.0.0:80 is also listener \"http-80\"'s"));
        assert!(e("listener \"a\" {\n  protocol  = \"http\"\n  addresses = [\"10.0.0.1\"]\n  port      = 80\n}\nlistener \"a\" {\n  protocol  = \"http\"\n  addresses = [\"10.0.0.2\"]\n  port      = 80\n}\n").contains("listener \"a\" is defined twice"));
        assert!(e("listener \"a b\" {\n  protocol  = \"http\"\n  addresses = [\"10.0.0.1\"]\n  port      = 80\n}\n").contains("a name is letters, digits"));
        // Different IPs on one port, or IPv4 and IPv6 wildcards, are separate sockets.
        with_pool("listener \"a\" {\n  protocol  = \"http\"\n  addresses = [\"10.0.0.1\", \"0.0.0.0\", \"::\"]\n  port      = 80\n}\n").validate().expect_err("a wildcard and an IP of its family share the port");
        with_pool("listener \"a\" {\n  protocol  = \"http\"\n  addresses = [\"10.0.0.1\", \"10.0.0.2\", \"::\"]\n  port      = 80\n}\n").validate().unwrap();
        assert!(e("listener \"https-443\" {\n  protocol  = \"https\"\n  addresses = [\"0.0.0.0\"]\n  port      = 443\n  tls_mode  = \"terminate\"\n}\n").contains("tcp listeners only"));
        let e = parse_err("listener \"q\" {\n  protocol  = \"quic\"\n  addresses = [\"0.0.0.0\"]\n  port      = 443\n}\n");
        assert!(e.contains("keel.hcl:2:3: unknown variant `quic`"), "{e}");
    }

    #[test]
    fn proxy_protocol_allowed_on_every_listener_kind() {
        for (protocol, body) in [("http", ""), ("https", ""), ("udp", "  pool = \"dns\"\n"), ("tcp", "  pool = \"dns\"\n")] {
            let head = format!("  protocol  = \"{protocol}\"\n  addresses = [\"0.0.0.0\"]\n  port      = 5353\n");
            with_pool(&format!("listener \"pp\" {{\n{head}{body}  proxy_protocol = true\n}}\n")).validate().expect(protocol);
        }
    }

    #[test]
    fn vhosts_take_a_pool_or_routes() {
        let cfg = with_pool("vhost \"a.test\" {\n  pool = \"dns\"\n  route \"/\" {\n    pool = \"dns\"\n  }\n}\n");
        assert!(err(&cfg).contains("pool and route blocks exclude each other"));
        let cfg = with_pool("vhost \"a.test\" {\n  route \"/a/\" {\n    pool = \"dns\"\n  }\n  route \"/a/\" {\n    pool = \"dns\"\n  }\n}\n");
        assert!(err(&cfg).contains("route \"/a/\" is defined twice"));
        let cfg = with_pool("vhost \"a.test\" {\n  route \"/api/\" {\n    pool = \"dns\"\n    cache {\n      enabled = true\n      ttl     = \"1d\"\n    }\n  }\n}\n");
        cfg.validate().unwrap();
        assert_eq!(cfg.vhosts[0].routes[0].cache.as_ref().unwrap().ttl, Some(Duration::from_secs(86_400)));
    }

    fn health(hc: &str) -> Config {
        with_pool(&format!("pool \"p\" {{\n  backend \"10.0.0.1:80\" {{}}\n  {hc}\n}}\n"))
    }

    fn health_err(hc: &str) -> String {
        parse_err(&format!("pool \"p\" {{\n  backend \"10.0.0.1:80\" {{}}\n  {hc}\n}}\n"))
    }

    #[test]
    fn health_check_tcp_and_http_keep_their_shape() {
        let cfg = health("health_check \"tcp\" {\n    interval = \"5s\"\n  }");
        let hc = cfg.pools["p"].health_check.as_ref().unwrap();
        assert!(matches!(hc.probe, ProbeConfig::Tcp));
        assert_eq!((hc.interval, hc.timeout), (Duration::from_secs(5), Duration::from_secs(2)));
        assert_eq!(hc.healthy_threshold, 2);
        cfg.validate().expect("valid");

        let cfg = health("health_check \"http\" {\n    path          = \"/ready\"\n    expect_status = [200, 204]\n    port          = 9000\n  }");
        let hc = cfg.pools["p"].health_check.as_ref().unwrap();
        match &hc.probe {
            ProbeConfig::Http { path, expect_status, tls, .. } => {
                assert_eq!(path, "/ready");
                assert_eq!(expect_status, &[StatusRange { lo: 200, hi: 200 }, StatusRange { lo: 204, hi: 204 }]);
                assert!(!tls);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hc.interval, Duration::from_secs(10));
        assert_eq!(hc.port, Some(9000));
        cfg.validate().expect("valid");
    }

    #[test]
    fn health_check_rejects_attributes_of_other_types() {
        assert!(health_err("health_check \"tcp\" {\n    path = \"/health\"\n  }").contains("'path' is not valid for type 'tcp'"));
        assert!(health_err("health_check \"udp\" {\n    expect_body = \"ok\"\n  }").contains("'expect_body' is not valid for type 'udp'"));
        assert!(health_err("health_check \"smtp\" {}").contains("not one of"));
    }

    #[test]
    fn expect_status_takes_codes_strings_and_ranges() {
        let cfg = health("health_check \"http\" {\n    expect_status = [200, \"204\", \"300-308\", \" 404 - 404 \"]\n  }");
        let ProbeConfig::Http { expect_status, .. } = &cfg.pools["p"].health_check.as_ref().unwrap().probe else { panic!() };
        let r = |lo, hi| StatusRange { lo, hi };
        assert_eq!(expect_status, &[r(200, 200), r(204, 204), r(300, 308), r(404, 404)]);
        let range = r(300, 308);
        assert!(range.contains(300) && range.contains(308) && !range.contains(299) && !range.contains(309));
        for (entry, wanted) in [
            ("\"210-201\"", "210 is above 201"),
            ("\"2xx\"", "is not a status code or a range"),
            ("\"100-600\"", "status codes are 100-599"),
            ("\"-\"", "is not a status code"),
            ("true", "is not a status code or a range"),
            ("99", "status codes are 100-599"),
        ] {
            let e = health_err(&format!("health_check \"http\" {{\n    expect_status = [{entry}]\n  }}"));
            assert!(e.contains(wanted), "{entry}: {e}");
            assert!(e.starts_with("keel.hcl:"), "located: {e}");
        }
    }

    #[test]
    fn dns_ntp_icmp_and_tls_probes_parse_and_validate() {
        let cfg = health("health_check \"dns\" {\n    query     = \"example.com\"\n    record    = \"AAAA\"\n    transport = \"tcp\"\n    expect    = \"2001:db8::1\"\n  }");
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
        assert!(matches!(health("health_check \"ntp\" {}").pools["p"].health_check.as_ref().unwrap().probe, ProbeConfig::Ntp));
        let cfg = health("health_check \"icmp\" {\n    timeout = \"1s\"\n  }");
        assert!(matches!(cfg.pools["p"].health_check.as_ref().unwrap().probe, ProbeConfig::Icmp));
        let cfg = health("health_check \"tls\" {\n    sni       = \"db.example.com\"\n    min_valid = \"14d\"\n  }");
        match &cfg.pools["p"].health_check.as_ref().unwrap().probe {
            ProbeConfig::Tls { sni, min_valid } => {
                assert_eq!(sni.as_deref(), Some("db.example.com"));
                assert_eq!(*min_valid, Some(Duration::from_secs(14 * 86_400)));
            }
            other => panic!("unexpected {other:?}"),
        }
        cfg.validate().expect("valid");
        assert!(err(&health("health_check \"tls\" {\n    sni = \"a..b\"\n  }")).contains("not a valid host name"));
        assert!(health_err("health_check \"tls\" {\n    path = \"/\"\n  }").contains("'path' is not valid for type 'tls'"));

        let e = |hc: &str| err(&health(hc));
        assert!(e("health_check \"dns\" {\n    query = \"\"\n  }").contains("not a valid DNS name"));
        assert!(e("health_check \"dns\" {\n    query = \"a..b\"\n  }").contains("not a valid DNS name"));
        assert!(e("health_check \"dns\" {\n    query  = \"example.com\"\n    expect = \"nope\"\n  }").contains("not an IP address"));
        assert!(e("health_check \"dns\" {\n    query  = \"example.com\"\n    expect = \"::1\"\n  }").contains("does not match record type"));
        assert!(health_err("health_check \"dns\" {}").contains("query"));
        assert!(health_err("health_check \"ntp\" {\n    query = \"x\"\n  }").contains("'query' is not valid for type 'ntp'"));
    }

    #[test]
    fn health_check_and_passive_validate_values() {
        assert!(health_err("health_check \"tcp\" {\n    interval = \"soon\"\n  }").contains("'soon' is not a duration"));
        assert!(health_err("health_check \"tcp\" {\n    timeout = \"500ms\"\n  }").contains("'500ms' is not a duration"));
        assert!(health_err("health_check \"tcp\" {\n    timeout = \"0s\"\n  }").contains("at least 1s"));
        let e = |hc: &str| err(&health(hc));
        assert!(e("health_check \"tcp\" {\n    healthy_threshold = 0\n  }").contains("thresholds"));
        assert!(e("health_check \"http\" {\n    path = \"health\"\n  }").contains("start with '/'"));
        assert!(health_err("health_check \"http\" {\n    expect_status = [42]\n  }").contains("expect_status entry 42: status codes are 100-599"));
        assert!(e("passive {\n    failures = 0\n  }").contains("passive.failures"));
        assert!(health_err("passive {\n    eject_for = \"forever\"\n  }").contains("'forever' is not a duration"));
    }

    #[test]
    fn durations_parse_strictly() {
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("30d").unwrap(), Duration::from_secs(30 * 86_400));
        assert_eq!(parse_duration("2w").unwrap(), Duration::from_secs(14 * 86_400));
        assert_eq!(parse_duration("1y").unwrap(), Duration::from_secs(365 * 86_400));
        for bad in ["10", "fast", "0s", "500ms", "1.5s", " 1s", "1 s", "-1s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(RenewBefore::parse("20d").unwrap(), RenewBefore::Before(Duration::from_secs(20 * 86_400)));
        assert!(RenewBefore::parse("20").is_err());
    }

    fn tcp_tls(listener: &str, certificate: &str) -> Config {
        with_pool(&format!("listener \"cache\" {{\n  protocol  = \"tcp\"\n  addresses = [\"0.0.0.0\"]\n  port      = 6379\n  pool      = \"dns\"\n{listener}}}\ncertificate \"cache.example.com\" {{\n{certificate}}}\n"))
    }

    #[test]
    fn tcp_tls_modes_validate() {
        let byo = "  cert = \"/c.pem\"\n  key  = \"/k.pem\"\n";
        tcp_tls("  tls_mode = \"terminate\"\n  tls_host = \"cache.example.com\"\n", byo).validate().expect("terminate ok");
        tcp_tls("  tls_mode   = \"reencrypt\"\n  tls_host   = \"cache.example.com\"\n  tls_verify = true\n  tls_ca     = \"/ca.pem\"\n", byo)
            .validate()
            .expect("reencrypt ok");
        let cfg = tcp_tls("", byo);
        assert_eq!(cfg.listeners[0].tls_mode, TlsMode::Passthrough);
        assert!(!cfg.certificates[0].is_acme());
        cfg.validate().expect("passthrough default ok");

        let e = |l: &str, c: &str| err(&tcp_tls(l, c));
        assert!(e("  tls_mode = \"terminate\"\n", byo).contains("needs tls_host"));
        assert!(e("  tls_mode = \"terminate\"\n  tls_host = \"other.example.com\"\n", byo).contains("matches no certificate"));
        assert!(e("  tls_host = \"cache.example.com\"\n", byo).contains("need tls_mode terminate or reencrypt"));
        assert!(e("  tls_mode   = \"terminate\"\n  tls_host   = \"cache.example.com\"\n  tls_verify = true\n", byo).contains("reencrypt only"));
        assert!(e("  tls_mode = \"reencrypt\"\n  tls_host = \"cache.example.com\"\n  tls_ca   = \"/ca.pem\"\n", byo).contains("needs tls_verify"));
        assert!(e("", "  cert = \"/c.pem\"\n").contains("set together"));
    }

    #[test]
    fn backend_weight_must_be_1_to_1000() {
        let mut cfg = with_pool("");
        for weight in [1, 1000] {
            cfg.pools.get_mut("dns").unwrap().backends[0].weight = weight;
            cfg.validate().expect("weight in range");
        }
        for weight in [0, 1001] {
            cfg.pools.get_mut("dns").unwrap().backends[0].weight = weight;
            assert!(err(&cfg).contains("weight must be 1-1000"), "weight {weight}");
        }
    }

    #[test]
    fn trusted_proxies_are_cidrs() {
        let cfg = with_pool("vhost \"a\" {\n  pool = \"dns\"\n  forwarded_headers {\n    mode            = \"append\"\n    trusted_proxies = [\"10.0.0.0/8\", \"nope\"]\n  }\n}\n");
        assert!(err(&cfg).contains("invalid CIDR 'nope'"));
    }

    #[test]
    fn cache_sizes_are_checked_at_load() {
        assert!(err(&with_pool("cache {\n  memory = \"10X\"\n}\n")).contains("cache.memory: unknown size unit 'X'"));
        assert!(err(&with_pool("cache {\n  disk {\n    path = \"/c\"\n    size = \"lots\"\n  }\n}\n")).contains("cache.disk.size"));
        with_pool("cache {\n  memory = \"256M\"\n}\n").validate().unwrap();
    }

    #[test]
    fn udp_limits() {
        let mut cfg = with_pool("");
        cfg.keel.udp_max_flows = 0;
        assert!(err(&cfg).contains("udp_max_flows"));
        assert!(parse_err("keel {\n  udp_flow_timeout = \"0s\"\n}\n").contains("at least 1s"));
    }
}
