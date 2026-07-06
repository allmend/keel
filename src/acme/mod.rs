//! Automatic TLS via ACME (Let's Encrypt or any ACME v2 CA), HTTP-01 only.
//!
//! Certificates come from named **issuers** (`acme.issuers`). A vhost opts in
//! with `tls: { acme: true }` (the issuer named "default", implicitly Let's
//! Encrypt) or `tls: { acme: <issuer-name> }`. Each issuer keeps its own ACME
//! account; hostnames map to exactly one issuer.
//!
//! Managed hostnames come from two places:
//!   - vhosts with `tls.acme` — issued certs are hot-swapped into the
//!     `CertStore` so Keel terminates TLS with them;
//!   - `acme.issuers.<name>.domains` — hostnames Keel does not terminate TLS
//!     for (plain TCP / TLS-passthrough backends). Keel answers the HTTP-01
//!     challenge and writes the cert/key files for the operator or backend to
//!     consume, the way Lego's standalone HTTP-01 mode works.
//!
//! Renewal: when less than `renew_before` of the certificate's lifetime
//! remains — a percentage ("30%", scales from 90-day to 6-day certs) or an
//! absolute window ("20d").
//!
//! Multi-process coordination (standalone mode runs one worker per process):
//! challenge tokens are files under `{storage}/challenges/`, so ANY worker can
//! answer the CA's validation request; issuance itself is serialized with an
//! exclusive flock on `{storage}/.issuer.lock` so exactly one worker talks to
//! the CAs. Every worker watches the cert files and hot-swaps renewed certs
//! into its own CertStore.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::config::{AcmeConfig, AcmeIssuer, Config, RenewBefore};
use crate::tls::{acme_cert_paths, CertStore};

/// How often each worker wakes to check certs (renewal window, file changes).
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// After a failed issuance, back off starting here, doubling per failure…
const FAILURE_BACKOFF_INITIAL: Duration = Duration::from_secs(60);
/// …up to this cap, so a broken domain cannot hammer the CA (rate limits).
const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(6 * 3600);
/// Cluster mode: how long the leader waits for a challenge token to reach
/// every node's log before telling the CA to validate. On timeout it proceeds
/// on the quorum guarantee alone — a node that is down is not serving port 80
/// traffic anyway, and blocking issuance on it forever would be worse.
const CHALLENGE_REPLICATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Replication confirms the entry is in each node's log; applying it (which
/// writes the token file) follows within a heartbeat. This grace covers that
/// gap before the CA is signalled.
const CHALLENGE_APPLY_GRACE: Duration = Duration::from_secs(1);

/// Only characters that appear in base64url ACME tokens — this is the guard
/// that keeps the challenge responder from ever touching another path.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 256
        && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Directory the proxy serves HTTP-01 responses from.
pub fn challenge_dir(storage: &str) -> PathBuf {
    Path::new(storage).join("challenges")
}

/// Account credentials wrapper persisted to `{storage}/{issuer}/account.json`.
/// The directory URL is recorded so pointing an issuer at a different CA
/// creates a fresh account instead of replaying credentials against the wrong
/// endpoint.
#[derive(Serialize, Deserialize)]
struct StoredAccount {
    directory: String,
    credentials: AccountCredentials,
}

/// One hostname Keel manages a certificate for.
struct ManagedCert {
    host: String,
    issuer: String,
    /// True when the cert is served by Keel's own TLS listeners (vhost);
    /// false for standalone `domains` entries (cert files only).
    vhost_managed: bool,
}

pub struct AcmeService {
    cfg: Config,
    acme: AcmeConfig,
    cert_store: Arc<CertStore>,
    managed: Vec<ManagedCert>,
    /// Present in cluster mode: certs replicate via Raft, only the leader
    /// talks to the CAs.
    cluster: Option<crate::cluster::ClusterHandle>,
}

impl AcmeService {
    /// Returns None when the config uses no ACME at all.
    pub fn from_config(
        cfg: &Config,
        cert_store: Arc<CertStore>,
        cluster: Option<crate::cluster::ClusterHandle>,
    ) -> Option<Self> {
        let acme = cfg.acme_effective()?;
        let managed: Vec<ManagedCert> = cfg
            .acme_assignments()
            .into_iter()
            .map(|(host, issuer, vhost_managed)| ManagedCert {
                host: host.to_owned(),
                issuer: issuer.to_owned(),
                vhost_managed,
            })
            .collect();
        if managed.is_empty() {
            return None;
        }
        Some(Self { cfg: cfg.clone(), acme, cert_store, managed, cluster })
    }
}

#[async_trait]
impl pingora::services::background::BackgroundService for AcmeService {
    async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
        // instant-acme's rustls needs a process-default crypto provider; in
        // standalone mode nothing else installs one (idempotent).
        let _ = rustls::crypto::ring::default_provider().install_default();

        if let Err(e) = prepare_storage(&self.acme.storage) {
            error!(error = %format!("{e:#}"), "acme: cannot prepare storage directory; ACME disabled");
            return;
        }
        for m in &self.managed {
            info!(host = m.host, issuer = m.issuer, vhost = m.vhost_managed, "acme: managing certificate");
        }

        // Cluster: mirror the replicated challenge map into the local
        // challenge directory, so THIS node answers HTTP-01 requests for
        // orders started by the leader. Every node runs this; the map is the
        // single source of truth for the directory in cluster mode.
        if let Some(cluster) = &self.cluster {
            let mut rx = cluster.challenges_rx.clone();
            let dir = challenge_dir(&self.acme.storage);
            let mut shutdown_c = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let map = rx.borrow_and_update().clone();
                    sync_challenge_dir(&dir, &map);
                    tokio::select! {
                        changed = rx.changed() => {
                            if changed.is_err() { break; }
                        }
                        _ = shutdown_c.changed() => {
                            if *shutdown_c.borrow() { break; }
                        }
                    }
                }
            });
        }

        let mut issuers = IssuerPool::new(&self.acme);
        let mut cert_mtimes: HashMap<String, SystemTime> = HashMap::new();

        loop {
            // Cluster: adopt any replicated cert that is better (longer valid)
            // than what this node has on disk — BEFORE the issuance check, so
            // a node never re-issues something the cluster already holds.
            self.pull_cluster_certs().await;

            // Only the leader talks to the CAs in cluster mode; standalone
            // always may. The flock additionally serializes across workers.
            if self.may_issue().await {
                match try_issuer_lock(&self.acme.storage) {
                    Ok(Some(_lock)) => {
                        for m in &self.managed {
                            if *shutdown.borrow() {
                                break;
                            }
                            issuers.maybe_issue(&m.host, &m.issuer, self.cluster.as_ref()).await;
                        }
                        // _lock drops here, releasing the flock.
                    }
                    Ok(None) => {} // another worker is the issuer this cycle
                    Err(e) => warn!(error = %format!("{e:#}"), "acme: issuer lock failed"),
                }
            }

            // Cluster leader: replicate any disk cert that is better than (or
            // missing from) the Raft state — covers fresh issuance and certs
            // that survived a full-cluster restart on disk only.
            self.push_cluster_certs().await;

            // Every worker: hot-swap certs that changed on disk (issued here,
            // by another worker, or adopted from the cluster).
            self.reload_changed_certs(&mut cert_mtimes);

            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tokio::time::sleep(CHECK_INTERVAL) => {}
            }
        }
    }
}

impl AcmeService {
    /// In cluster mode: true only on the current Raft leader.
    async fn may_issue(&self) -> bool {
        let Some(cluster) = &self.cluster else { return true };
        let Some(raft) = cluster.raft().await else { return false };
        let m = raft.metrics().borrow().clone();
        m.current_leader == Some(m.id)
    }

    /// Adopt replicated certs that are strictly better than the local disk
    /// copy. "Better" = valid with more remaining lifetime; the loser is
    /// overwritten so disk and Raft converge on one source of truth.
    async fn pull_cluster_certs(&self) {
        let Some(cluster) = &self.cluster else { return };
        let replicated = cluster.certs_rx.borrow().clone();
        for m in &self.managed {
            let Some((cert_pem, key_pem)) = replicated.get(&m.host) else { continue };
            let raft_remaining = match pem_remaining_secs(cert_pem) {
                Some(r) if r > 0 => r,
                _ => continue, // expired or unparsable — never adopt
            };
            let (cert_path, _) = acme_cert_paths(&self.acme.storage, &m.host);
            let disk_remaining = std::fs::read(&cert_path)
                .ok()
                .and_then(|pem| pem_remaining_secs(std::str::from_utf8(&pem).unwrap_or("")))
                .unwrap_or(0);
            if raft_remaining > disk_remaining {
                match write_cert_pair(&self.acme.storage, &m.host, cert_pem, key_pem) {
                    Ok(()) => info!(host = m.host, "acme: adopted replicated certificate from cluster"),
                    Err(e) => error!(host = m.host, error = %format!("{e:#}"), "acme: failed to write replicated certificate"),
                }
            }
        }
    }

    /// Leader only: replicate disk certs that beat (or are missing from) the
    /// Raft state, so followers and future joiners receive them.
    async fn push_cluster_certs(&self) {
        let Some(cluster) = &self.cluster else { return };
        let Some(raft) = cluster.raft().await else { return };
        {
            let m = raft.metrics().borrow().clone();
            if m.current_leader != Some(m.id) {
                return;
            }
        }
        let replicated = cluster.certs_rx.borrow().clone();
        for m in &self.managed {
            let (cert_path, key_path) = acme_cert_paths(&self.acme.storage, &m.host);
            let Ok(cert_pem) = std::fs::read_to_string(&cert_path) else { continue };
            let Ok(key_pem) = std::fs::read_to_string(&key_path) else { continue };
            let disk_remaining = match pem_remaining_secs(&cert_pem) {
                Some(r) if r > 0 => r,
                _ => continue, // never replicate an expired/broken cert
            };
            let raft_remaining = replicated
                .get(&m.host)
                .and_then(|(c, _)| pem_remaining_secs(c))
                .unwrap_or(0);
            if disk_remaining > raft_remaining {
                match crate::cluster::push_cert(&raft, m.host.clone(), cert_pem, key_pem).await {
                    Ok(()) => info!(host = m.host, "acme: certificate replicated to cluster"),
                    Err(e) => warn!(host = m.host, error = %format!("{e:#}"), "acme: certificate replication failed"),
                }
            }
        }
    }

    /// Reload the CertStore when any vhost-managed cert file changed on disk.
    fn reload_changed_certs(&self, seen: &mut HashMap<String, SystemTime>) {
        let mut changed = false;
        for m in self.managed.iter().filter(|m| m.vhost_managed) {
            let (cert_path, _) = acme_cert_paths(&self.acme.storage, &m.host);
            let Ok(meta) = std::fs::metadata(&cert_path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if seen.insert(m.host.clone(), mtime) != Some(mtime) {
                changed = true;
            }
        }
        if changed {
            match self.cert_store.reload(&self.cfg) {
                Ok(()) => info!("acme: certificates hot-swapped into TLS store"),
                Err(e) => error!(error = %format!("{e:#}"), "acme: cert store reload failed"),
            }
        }
    }
}

/// Make the on-disk challenge directory mirror the replicated challenge map
/// exactly: write tokens that are missing or differ, delete token-shaped files
/// that are no longer in the map. Cluster mode only — there the map is the
/// single source of truth for the directory.
///
/// Token names arrive via the Raft log and are re-validated here; a node never
/// trusts replicated data with a filesystem path.
fn sync_challenge_dir(dir: &Path, map: &crate::cluster::types::ChallengeMap) {
    for (token, key_auth) in map {
        if !valid_token(token) {
            warn!(token, "acme: replicated challenge token failed validation; skipped");
            continue;
        }
        let path = dir.join(token);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(key_auth.as_str()) {
            if let Err(e) = std::fs::write(&path, key_auth) {
                warn!(token, error = %e, "acme: cannot write replicated challenge token");
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if valid_token(name) && !map.contains_key(name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn prepare_storage(storage: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(storage).with_context(|| format!("create {storage}"))?;
    // Private keys live here — owner only.
    std::fs::set_permissions(storage, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {storage}"))?;
    let challenges = challenge_dir(storage);
    std::fs::create_dir_all(&challenges)
        .with_context(|| format!("create {}", challenges.display()))?;
    // Tokens are public by definition; workers may run as different users.
    std::fs::set_permissions(&challenges, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Take the cross-process issuer lock non-blockingly. Returns the held lock
/// (released on drop) or None if another worker has it.
fn try_issuer_lock(storage: &str) -> Result<Option<nix::fcntl::Flock<std::fs::File>>> {
    let path = Path::new(storage).join(".issuer.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, e)) => Err(anyhow::anyhow!("flock: {e}")),
    }
}

// Issuer pool — the part that talks to the CAs

struct IssuerPool {
    storage: String,
    global_renew: RenewBefore,
    issuers: HashMap<String, AcmeIssuer>,
    /// Lazily created ACME accounts, one per issuer.
    accounts: HashMap<String, Account>,
    /// Per-host failure backoff: (next attempt allowed at, current delay).
    backoff: HashMap<String, (Instant, Duration)>,
}

impl IssuerPool {
    fn new(acme: &AcmeConfig) -> Self {
        // Validated at config load; fall back to 30% defensively.
        let global_renew =
            RenewBefore::parse(&acme.renew_before).unwrap_or(RenewBefore::Percent(30));
        Self {
            storage: acme.storage.clone(),
            global_renew,
            issuers: acme.issuers.clone(),
            accounts: HashMap::new(),
            backoff: HashMap::new(),
        }
    }

    fn issuer(&self, name: &str) -> AcmeIssuer {
        self.issuers.get(name).cloned().unwrap_or_default()
    }

    fn renew_before(&self, issuer: &AcmeIssuer) -> RenewBefore {
        issuer
            .renew_before
            .as_deref()
            .and_then(|s| RenewBefore::parse(s).ok())
            .unwrap_or(self.global_renew)
    }

    /// Issue or renew `host` via `issuer_name` if due, honoring the backoff.
    async fn maybe_issue(
        &mut self,
        host: &str,
        issuer_name: &str,
        cluster: Option<&crate::cluster::ClusterHandle>,
    ) {
        let issuer = self.issuer(issuer_name);
        if !needs_issuance(&self.storage, host, self.renew_before(&issuer)) {
            return;
        }
        if let Some((next_at, _)) = self.backoff.get(host) {
            if Instant::now() < *next_at {
                return;
            }
        }

        match self.issue(host, issuer_name, &issuer, cluster).await {
            Ok(()) => {
                self.backoff.remove(host);
                info!(host, issuer = issuer_name, "acme: certificate issued");
            }
            Err(e) => {
                let delay = self
                    .backoff
                    .get(host)
                    .map(|(_, d)| (*d * 2).min(FAILURE_BACKOFF_MAX))
                    .unwrap_or(FAILURE_BACKOFF_INITIAL);
                self.backoff.insert(host.to_owned(), (Instant::now() + delay, delay));
                error!(
                    host,
                    issuer = issuer_name,
                    retry_in_secs = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "acme: issuance failed"
                );
            }
        }
    }

    async fn issue(
        &mut self,
        host: &str,
        issuer_name: &str,
        issuer: &AcmeIssuer,
        cluster: Option<&crate::cluster::ClusterHandle>,
    ) -> Result<()> {
        let account = self.account(issuer_name, issuer).await?;

        let identifier = Identifier::Dns(host.to_owned());
        let mut order = account
            .new_order(&NewOrder::new(std::slice::from_ref(&identifier)))
            .await
            .context("new order")?;

        // Publish HTTP-01 responses for all pending authorizations, then tell
        // the CA to validate. Tokens are cleaned up when we're done, pass or fail.
        //
        // Standalone: the token is written straight to the challenge dir.
        // Cluster: the token is committed to the Raft log and the challenge
        // sync task writes the file on every node (including this one); the CA
        // is signalled only after every node is confirmed to hold the token,
        // because it may validate from multiple vantage points that can reach
        // any node.
        let mut published: Vec<PathBuf> = Vec::new();
        let mut cluster_tokens: Vec<String> = Vec::new();
        let result = async {
            let mut authorizations = order.authorizations();
            let mut ready = 0usize;
            while let Some(authz) = authorizations.next().await {
                let mut authz = authz.context("authorization")?;
                match authz.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => anyhow::bail!("authorization in unexpected state {status:?}"),
                }
                let mut challenge = authz
                    .challenge(ChallengeType::Http01)
                    .context("CA offered no http-01 challenge")?;
                let token = challenge.token.clone();
                if !valid_token(&token) {
                    anyhow::bail!("CA sent a token with unexpected characters");
                }
                let key_auth = challenge.key_authorization();
                if let Some(cluster) = cluster {
                    self.replicate_challenge(cluster, &token, key_auth.as_str()).await?;
                    cluster_tokens.push(token);
                } else {
                    let token_path = challenge_dir(&self.storage).join(&token);
                    std::fs::write(&token_path, key_auth.as_str())
                        .with_context(|| format!("write {}", token_path.display()))?;
                    published.push(token_path);
                }
                ready += 1;
                challenge.set_ready().await.context("set challenge ready")?;
            }
            drop(authorizations);
            info!(host, tokens = ready, "acme: http-01 challenges published");

            let status = order.poll_ready(&RetryPolicy::default()).await.context("poll order")?;
            if status != OrderStatus::Ready {
                anyhow::bail!("order ended in state {status:?} (is port 80 for {host} reaching this Keel?)");
            }

            let key_pem = order.finalize().await.context("finalize order")?;
            let cert_pem = order
                .poll_certificate(&RetryPolicy::default())
                .await
                .context("download certificate")?;
            write_cert_pair(&self.storage, host, &cert_pem, &key_pem)
        }
        .await;

        for p in published {
            let _ = std::fs::remove_file(p);
        }
        // Retract replicated tokens; the sync task removes the files on every
        // node. Best effort — if leadership was lost mid-order this fails and
        // the tokens linger in the map until a later issuance cycle; a stale
        // token is served but validates nothing.
        if !cluster_tokens.is_empty() {
            if let Some(raft) = match cluster {
                Some(c) => c.raft().await,
                None => None,
            } {
                for token in cluster_tokens {
                    if let Err(e) = crate::cluster::remove_challenge(&raft, token).await {
                        warn!(host, error = %format!("{e:#}"), "acme: challenge retraction failed");
                    }
                }
            }
        }
        result
    }

    /// Commit a challenge token to the Raft log and confirm every node holds
    /// it before returning, so the CA can validate against any node. Commit
    /// alone only proves a quorum has the entry.
    async fn replicate_challenge(
        &self,
        cluster: &crate::cluster::ClusterHandle,
        token: &str,
        key_auth: &str,
    ) -> Result<()> {
        let raft = cluster.raft().await.context("cluster raft not initialized")?;
        let index =
            crate::cluster::push_challenge(&raft, token.to_owned(), key_auth.to_owned()).await?;
        match crate::cluster::wait_replicated_to_all(&raft, index, CHALLENGE_REPLICATION_TIMEOUT)
            .await
        {
            Ok(()) => {
                // Every node has the entry in its log; applying it (the file
                // write) follows within a heartbeat.
                tokio::time::sleep(CHALLENGE_APPLY_GRACE).await;
                info!(token, "acme: challenge replicated to all cluster nodes");
            }
            Err(lagging) => warn!(
                token,
                lagging = ?lagging,
                "acme: challenge not confirmed on all nodes within {}s; \
                 proceeding — quorum has it",
                CHALLENGE_REPLICATION_TIMEOUT.as_secs()
            ),
        }
        // Belt and braces: the local file is written by the same sync task
        // pipeline as on followers. Seeing it appear confirms that pipeline
        // end-to-end before the CA is signalled.
        let local = challenge_dir(&self.storage).join(token);
        for _ in 0..20 {
            if local.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("challenge token committed but never appeared in {}", local.display())
    }

    /// Load the persisted ACME account for `issuer_name`, or register one.
    /// Reused across all renewals to stay within CA rate limits.
    async fn account(&mut self, issuer_name: &str, issuer: &AcmeIssuer) -> Result<&Account> {
        if !self.accounts.contains_key(issuer_name) {
            let dir = Path::new(&self.storage).join(issuer_name);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            let path = dir.join("account.json");

            let stored: Option<StoredAccount> = std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .filter(|s: &StoredAccount| s.directory == issuer.directory);

            let account = match stored {
                Some(s) => builder(issuer)?
                    .from_credentials(s.credentials)
                    .await
                    .context("restore ACME account")?,
                None => {
                    let contact: Vec<String> =
                        issuer.email.iter().map(|e| format!("mailto:{e}")).collect();
                    let contact_refs: Vec<&str> = contact.iter().map(String::as_str).collect();
                    let (account, credentials) = builder(issuer)?
                        .create(
                            &NewAccount {
                                contact: &contact_refs,
                                terms_of_service_agreed: true,
                                only_return_existing: false,
                            },
                            issuer.directory.clone(),
                            None,
                        )
                        .await
                        .context("create ACME account")?;
                    let stored =
                        StoredAccount { directory: issuer.directory.clone(), credentials };
                    write_private(&path, serde_json::to_string(&stored)?.as_bytes())
                        .context("persist ACME account")?;
                    info!(issuer = issuer_name, directory = issuer.directory, "acme: account registered");
                    account
                }
            };
            self.accounts.insert(issuer_name.to_owned(), account);
        }
        Ok(self.accounts.get(issuer_name).unwrap())
    }
}

fn builder(issuer: &AcmeIssuer) -> Result<instant_acme::AccountBuilder> {
    Ok(match &issuer.root_ca {
        Some(pem) => Account::builder_with_root(pem)
            .with_context(|| format!("load root_ca '{pem}'"))?,
        None => Account::builder().context("build ACME http client")?,
    })
}

/// True when the cert is missing, unreadable, or inside the renewal window.
fn needs_issuance(storage: &str, host: &str, renew: RenewBefore) -> bool {
    let (cert_path, key_path) = acme_cert_paths(storage, host);
    if !Path::new(&cert_path).exists() || !Path::new(&key_path).exists() {
        return true;
    }
    let Ok(pem) = std::fs::read(&cert_path) else { return true };
    let Ok(cert) = openssl::x509::X509::from_pem(&pem) else {
        warn!(host, "acme: existing certificate unparsable; reissuing");
        return true;
    };
    match remaining_and_lifetime_secs(&cert) {
        Some((remaining, lifetime)) => {
            let threshold = match renew {
                RenewBefore::Percent(p) => lifetime * i64::from(p) / 100,
                RenewBefore::Days(d) => i64::from(d) * 86_400,
            };
            remaining < threshold
        }
        None => {
            warn!(host, "acme: cannot read certificate validity; reissuing");
            true
        }
    }
}

/// Seconds until a PEM certificate's notAfter; negative when expired,
/// None when unparsable.
fn pem_remaining_secs(cert_pem: &str) -> Option<i64> {
    let cert = openssl::x509::X509::from_pem(cert_pem.as_bytes()).ok()?;
    remaining_and_lifetime_secs(&cert).map(|(r, _)| r)
}

/// (seconds until notAfter, total lifetime in seconds) — negative remaining
/// means the certificate is already expired.
fn remaining_and_lifetime_secs(cert: &openssl::x509::X509) -> Option<(i64, i64)> {
    let now = openssl::asn1::Asn1Time::days_from_now(0).ok()?;
    let remaining = now.diff(cert.not_after()).ok()?;
    let lifetime = cert.not_before().diff(cert.not_after()).ok()?;
    let to_secs = |d: openssl::asn1::TimeDiff| i64::from(d.days) * 86_400 + i64::from(d.secs);
    Some((to_secs(remaining), to_secs(lifetime)))
}

/// Write key (0600) then cert (0644), each atomically via temp + rename, so a
/// crash mid-write never leaves a torn pair with a fresh mtime.
fn write_cert_pair(storage: &str, host: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
    let (cert_path, key_path) = acme_cert_paths(storage, host);
    write_private(Path::new(&key_path), key_pem.as_bytes())?;
    let tmp = format!("{cert_path}.tmp");
    std::fs::write(&tmp, cert_pem).with_context(|| format!("write {tmp}"))?;
    std::fs::rename(&tmp, &cert_path).with_context(|| format!("rename to {cert_path}"))?;
    Ok(())
}

/// Atomic write with 0600 permissions (private keys, account credentials).
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::valid_token;
    use crate::config::RenewBefore;

    #[test]
    fn token_charset() {
        assert!(valid_token("evaGxfADs6pSRb2LAv9IZf17Dt3juxGJ-PCt92wr-oA"));
        assert!(!valid_token(""));
        assert!(!valid_token("../../etc/passwd"));
        assert!(!valid_token("a/b"));
        assert!(!valid_token("a.b"));
        assert!(!valid_token(&"x".repeat(300)));
    }

    #[test]
    fn challenge_dir_mirrors_replicated_map() {
        let dir = std::env::temp_dir().join(format!("keel-chal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Stale token from a completed order + a file that must survive.
        std::fs::write(dir.join("staletoken"), "old").unwrap();
        std::fs::write(dir.join("not-a-token!"), "keep").unwrap();

        let mut map = crate::cluster::types::ChallengeMap::new();
        map.insert("newtoken-123_abc".into(), "newtoken-123_abc.keyauth".into());
        map.insert("../../etc/passwd".into(), "attack".into());
        super::sync_challenge_dir(&dir, &map);

        assert_eq!(
            std::fs::read_to_string(dir.join("newtoken-123_abc")).unwrap(),
            "newtoken-123_abc.keyauth"
        );
        assert!(!dir.join("staletoken").exists(), "stale token must be removed");
        assert!(dir.join("not-a-token!").exists(), "non-token files must be untouched");
        // The traversal "token" must have been skipped, not written anywhere:
        // the dir holds exactly the valid token and the non-token file.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renew_before_parsing() {
        assert_eq!(RenewBefore::parse("30%").unwrap(), RenewBefore::Percent(30));
        assert_eq!(RenewBefore::parse(" 15% ").unwrap(), RenewBefore::Percent(15));
        assert_eq!(RenewBefore::parse("20d").unwrap(), RenewBefore::Days(20));
        assert!(RenewBefore::parse("0%").is_err());
        assert!(RenewBefore::parse("95%").is_err());
        assert!(RenewBefore::parse("0d").is_err());
        assert!(RenewBefore::parse("30").is_err());
        assert!(RenewBefore::parse("").is_err());
    }
}
