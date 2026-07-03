//! Automatic TLS via ACME (Let's Encrypt or any ACME v2 CA), HTTP-01 only.
//!
//! Managed hostnames come from two places:
//!   - vhosts with `tls.acme: true` — issued certs are hot-swapped into the
//!     `CertStore` so Keel terminates TLS with them;
//!   - `acme.domains` — hostnames Keel does not terminate TLS for (plain TCP /
//!     TLS-passthrough backends). Keel answers the HTTP-01 challenge and writes
//!     the cert/key files for the operator or backend to consume, the way
//!     Lego's standalone HTTP-01 mode works.
//!
//! Multi-process coordination (standalone mode runs one worker per process):
//! challenge tokens are files under `{storage}/challenges/`, so ANY worker can
//! answer the CA's validation request; issuance itself is serialized with an
//! exclusive flock on `{storage}/.issuer.lock` so exactly one worker talks to
//! the CA. Every worker watches the cert files and hot-swaps renewed certs
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

use crate::config::{AcmeConfig, Config};
use crate::tls::{acme_cert_paths, CertStore};

/// How often each worker wakes to check certs (renewal window, file changes).
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Renew when the certificate expires within this window.
const RENEW_BEFORE_DAYS: u32 = 30;
/// After a failed issuance, back off starting here, doubling per failure…
const FAILURE_BACKOFF_INITIAL: Duration = Duration::from_secs(60);
/// …up to this cap, so a broken domain cannot hammer the CA (rate limits).
const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(6 * 3600);

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

/// Account credentials wrapper persisted to `{storage}/account.json`. The
/// directory URL is recorded so switching CAs creates a fresh account instead
/// of replaying credentials against the wrong endpoint.
#[derive(Serialize, Deserialize)]
struct StoredAccount {
    directory: String,
    credentials: AccountCredentials,
}

pub struct AcmeService {
    cfg: Config,
    acme: AcmeConfig,
    cert_store: Arc<CertStore>,
    /// Hosts whose certs are terminated by Keel (present as vhosts).
    vhost_hosts: Vec<String>,
    /// All managed hosts (vhost hosts + standalone acme.domains).
    all_hosts: Vec<String>,
}

impl AcmeService {
    /// Returns None when the config uses no ACME at all.
    pub fn from_config(cfg: &Config, cert_store: Arc<CertStore>) -> Option<Self> {
        let acme = cfg.acme_effective()?;
        let all_hosts = cfg.acme_hosts();
        if all_hosts.is_empty() {
            return None;
        }
        let vhost_hosts = cfg
            .vhosts
            .iter()
            .filter(|v| v.tls.as_ref().map_or(false, |t| t.acme))
            .map(|v| v.host.clone())
            .collect();
        Some(Self { cfg: cfg.clone(), acme, cert_store, vhost_hosts, all_hosts })
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
        info!(
            hosts = ?self.all_hosts,
            directory = self.acme.directory,
            storage = self.acme.storage,
            "acme: managing certificates"
        );

        let mut issuer = Issuer::new(&self.acme);
        let mut cert_mtimes: HashMap<String, SystemTime> = HashMap::new();

        loop {
            // Every worker: hot-swap certs that changed on disk (issued or
            // renewed by whichever worker holds the issuer lock).
            self.reload_changed_certs(&mut cert_mtimes);

            // One worker at a time: issue/renew what needs it.
            match try_issuer_lock(&self.acme.storage) {
                Ok(Some(_lock)) => {
                    for host in &self.all_hosts {
                        if *shutdown.borrow() {
                            break;
                        }
                        issuer.maybe_issue(host).await;
                    }
                    // _lock drops here, releasing the flock.
                }
                Ok(None) => {} // another worker is the issuer this cycle
                Err(e) => warn!(error = %format!("{e:#}"), "acme: issuer lock failed"),
            }

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
    /// Reload the CertStore when any vhost-managed cert file changed on disk.
    fn reload_changed_certs(&self, seen: &mut HashMap<String, SystemTime>) {
        let mut changed = false;
        for host in &self.vhost_hosts {
            let (cert_path, _) = acme_cert_paths(&self.acme.storage, host);
            let Ok(meta) = std::fs::metadata(&cert_path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if seen.insert(host.clone(), mtime) != Some(mtime) {
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

// Issuer — the part that talks to the CA

struct Issuer {
    acme: AcmeConfig,
    account: Option<Account>,
    /// Per-host failure backoff: (next attempt allowed at, current delay).
    backoff: HashMap<String, (Instant, Duration)>,
}

impl Issuer {
    fn new(acme: &AcmeConfig) -> Self {
        Self { acme: acme.clone(), account: None, backoff: HashMap::new() }
    }

    /// Issue or renew `host` if due, honoring the failure backoff.
    async fn maybe_issue(&mut self, host: &str) {
        if !needs_issuance(&self.acme.storage, host) {
            return;
        }
        if let Some((next_at, _)) = self.backoff.get(host) {
            if Instant::now() < *next_at {
                return;
            }
        }

        match self.issue(host).await {
            Ok(()) => {
                self.backoff.remove(host);
                info!(host, "acme: certificate issued");
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
                    retry_in_secs = delay.as_secs(),
                    error = %format!("{e:#}"),
                    "acme: issuance failed"
                );
            }
        }
    }

    async fn issue(&mut self, host: &str) -> Result<()> {
        let account = self.account().await?;

        let identifier = Identifier::Dns(host.to_owned());
        let mut order = account
            .new_order(&NewOrder::new(std::slice::from_ref(&identifier)))
            .await
            .context("new order")?;

        // Publish HTTP-01 responses for all pending authorizations, then tell
        // the CA to validate. Tokens are cleaned up when we're done, pass or fail.
        let mut published: Vec<PathBuf> = Vec::new();
        let result = async {
            let mut authorizations = order.authorizations();
            let mut ready: Vec<String> = Vec::new();
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
                let token_path = challenge_dir(&self.acme.storage).join(&token);
                std::fs::write(&token_path, key_auth.as_str())
                    .with_context(|| format!("write {}", token_path.display()))?;
                published.push(token_path);
                ready.push(token);
                challenge.set_ready().await.context("set challenge ready")?;
            }
            drop(authorizations);
            info!(host, tokens = ready.len(), "acme: http-01 challenges published");

            let status = order.poll_ready(&RetryPolicy::default()).await.context("poll order")?;
            if status != OrderStatus::Ready {
                anyhow::bail!("order ended in state {status:?} (is port 80 for {host} reaching this Keel?)");
            }

            let key_pem = order.finalize().await.context("finalize order")?;
            let cert_pem = order
                .poll_certificate(&RetryPolicy::default())
                .await
                .context("download certificate")?;
            write_cert_pair(&self.acme.storage, host, &cert_pem, &key_pem)
        }
        .await;

        for p in published {
            let _ = std::fs::remove_file(p);
        }
        result
    }

    /// Load the persisted ACME account, or register one. Reused across all
    /// renewals to stay within CA rate limits.
    async fn account(&mut self) -> Result<&Account> {
        if self.account.is_none() {
            let path = Path::new(&self.acme.storage).join("account.json");

            let stored: Option<StoredAccount> = std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .filter(|s: &StoredAccount| s.directory == self.acme.directory);

            let account = match stored {
                Some(s) => self
                    .builder()?
                    .from_credentials(s.credentials)
                    .await
                    .context("restore ACME account")?,
                None => {
                    let contact: Vec<String> =
                        self.acme.email.iter().map(|e| format!("mailto:{e}")).collect();
                    let contact_refs: Vec<&str> = contact.iter().map(String::as_str).collect();
                    let (account, credentials) = self
                        .builder()?
                        .create(
                            &NewAccount {
                                contact: &contact_refs,
                                terms_of_service_agreed: true,
                                only_return_existing: false,
                            },
                            self.acme.directory.clone(),
                            None,
                        )
                        .await
                        .context("create ACME account")?;
                    let stored =
                        StoredAccount { directory: self.acme.directory.clone(), credentials };
                    write_private(&path, serde_json::to_string(&stored)?.as_bytes())
                        .context("persist ACME account")?;
                    info!(directory = self.acme.directory, "acme: account registered");
                    account
                }
            };
            self.account = Some(account);
        }
        Ok(self.account.as_ref().unwrap())
    }

    fn builder(&self) -> Result<instant_acme::AccountBuilder> {
        Ok(match &self.acme.root_ca {
            Some(pem) => Account::builder_with_root(pem)
                .with_context(|| format!("load acme.root_ca '{pem}'"))?,
            None => Account::builder().context("build ACME http client")?,
        })
    }
}

/// True when the cert is missing, unreadable, or inside the renewal window.
fn needs_issuance(storage: &str, host: &str) -> bool {
    let (cert_path, key_path) = acme_cert_paths(storage, host);
    if !Path::new(&cert_path).exists() || !Path::new(&key_path).exists() {
        return true;
    }
    let Ok(pem) = std::fs::read(&cert_path) else { return true };
    let Ok(cert) = openssl::x509::X509::from_pem(&pem) else {
        warn!(host, "acme: existing certificate unparsable; reissuing");
        return true;
    };
    let Ok(renew_at) = openssl::asn1::Asn1Time::days_from_now(RENEW_BEFORE_DAYS) else {
        return false;
    };
    cert.not_after() < renew_at.as_ref()
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

    #[test]
    fn token_charset() {
        assert!(valid_token("evaGxfADs6pSRb2LAv9IZf17Dt3juxGJ-PCt92wr-oA"));
        assert!(!valid_token(""));
        assert!(!valid_token("../../etc/passwd"));
        assert!(!valid_token("a/b"));
        assert!(!valid_token("a.b"));
        assert!(!valid_token(&"x".repeat(300)));
    }
}
