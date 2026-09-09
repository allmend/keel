//! Persistent control CA — signs the remote control listener's server cert
//! and operator client certs (`keel credentials create`).
//!
//! Standalone and cluster mode both use this CA. It lives on disk in
//! `control.remote.ca_dir` (`ca.crt` / `ca.key`) and is generated on first
//! use, so `keel credentials create` and the remote listener can each come
//! first — whichever runs first creates it, the other loads it.

use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

/// SNI / server-cert SAN the client dials by. The keelconfig endpoint is the
/// TCP address; the TLS identity is this fixed name, verified against the
/// control CA — the same pattern as the cluster's "keel-cluster" identity.
pub const SERVER_NAME: &str = "keel-control";

pub struct ControlCa {
    issuer: Issuer<'static, KeyPair>,
    pub ca_cert_pem: String,
    /// Kept so cluster mode can replicate the CA through the Raft log.
    pub ca_key_pem: String,
}

impl ControlCa {
    /// Load the CA from `dir`, or generate and persist one (dir 0700,
    /// key 0600).
    pub fn load_or_generate(dir: &str) -> Result<Self> {
        let cert_path = Path::new(dir).join("ca.crt");
        let key_path = Path::new(dir).join("ca.key");

        if cert_path.exists() && key_path.exists() {
            let cert_pem = std::fs::read_to_string(&cert_path)
                .with_context(|| format!("read {}", cert_path.display()))?;
            let key_pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("read {}", key_path.display()))?;
            return Self::from_pems(cert_pem, key_pem);
        }

        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).with_context(|| format!("create {dir}"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;

        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(rcgen::DnType::CommonName, "Keel Control CA");
        let cert = params.self_signed(&key)?;
        let cert_pem = cert.pem();

        let key_pem = key.serialize_pem();
        write_file(&key_path, key_pem.as_bytes(), 0o600)?;
        write_file(&cert_path, cert_pem.as_bytes(), 0o644)?;
        tracing::info!(dir, "control: control CA generated");

        let issuer = Issuer::new(params, key);
        Ok(Self { issuer, ca_cert_pem: cert_pem, ca_key_pem: key_pem })
    }

    /// Write a CA received from the cluster into `dir`, replacing whatever
    /// was there, and load it. Same permissions as a generated one.
    pub fn install(dir: &str, cert_pem: &str, key_pem: &str) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).with_context(|| format!("create {dir}"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let ca = Self::from_pems(cert_pem.to_owned(), key_pem.to_owned())?;
        write_file(&Path::new(dir).join("ca.key"), key_pem.as_bytes(), 0o600)?;
        write_file(&Path::new(dir).join("ca.crt"), cert_pem.as_bytes(), 0o644)?;
        Ok(ca)
    }

    fn from_pems(cert_pem: String, key_pem: String) -> Result<Self> {
        let key = KeyPair::from_pem(&key_pem).context("parse control CA key")?;
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
            .map_err(|e| anyhow::anyhow!("parse control CA cert: {e}"))?;
        Ok(Self { issuer, ca_cert_pem: cert_pem, ca_key_pem: key_pem })
    }

    /// Issue the remote listener's server certificate (in memory only).
    pub fn issue_server(&self) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::NoCa;
        params.distinguished_name.push(rcgen::DnType::CommonName, SERVER_NAME);
        params.subject_alt_names = vec![rcgen::SanType::DnsName(
            SERVER_NAME.try_into().map_err(|e| anyhow::anyhow!("SAN: {e:?}"))?,
        )];
        let cert = params.signed_by(&key, &self.issuer)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Issue an operator client certificate. The CN is the operator name and
    /// appears in the audit log of every command they run.
    pub fn issue_client(&self, name: &str) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::NoCa;
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        let cert = params.signed_by(&key, &self.issuer)?;
        Ok((cert.pem(), key.serialize_pem()))
    }
}

fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_then_install_round_trips_through_files() {
        let dir = std::env::temp_dir().join(format!("keel-ca-{}", std::process::id()));
        let a = dir.join("a").to_string_lossy().into_owned();
        let b = dir.join("b").to_string_lossy().into_owned();
        let generated = ControlCa::load_or_generate(&a).unwrap();
        let loaded = ControlCa::load_or_generate(&a).unwrap();
        assert_eq!(generated.ca_cert_pem, loaded.ca_cert_pem, "second load reuses the files");

        // A follower installs the replicated pair and can issue with it.
        let installed = ControlCa::install(&b, &generated.ca_cert_pem, &generated.ca_key_pem).unwrap();
        assert_eq!(installed.ca_cert_pem, generated.ca_cert_pem);
        assert_eq!(std::fs::read_to_string(dir.join("b/ca.key")).unwrap(), generated.ca_key_pem);
        installed.issue_client("operator").unwrap();
        // Reloading from the installed files gives the same CA again.
        assert_eq!(ControlCa::load_or_generate(&b).unwrap().ca_cert_pem, generated.ca_cert_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
