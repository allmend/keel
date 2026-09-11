//! JWT validation (RFC 7519) for the `auth.jwt` gateway rule. Self-contained:
//! the key is configured, nothing is fetched. HS256/384/512 with a shared
//! secret, or RS256/384/512 and ES256/384 with a public key. The accepted
//! algorithms follow from the key: a secret only accepts HS*, a public key
//! only RS*/ES*, so a token cannot pick an algorithm the operator did not
//! intend, and `none` is never accepted.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Public};

use crate::config::JwtConfig;

/// Verification material derived once from config.
pub struct JwtVerifier {
    key: Key,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub leeway_secs: u64,
    /// (claim name, header name) pairs copied to the backend request.
    pub claim_headers: Vec<(String, String)>,
}

enum Key {
    Hmac(Vec<u8>),
    Public(PKey<Public>),
}

// Never print key material; the routing table derives Debug over its rules.
impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("key", &match self.key { Key::Hmac(_) => "hmac", Key::Public(_) => "public" })
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish()
    }
}

/// Why a token was refused. Short and stable: it is the metric label and the
/// `error` value in `WWW-Authenticate`.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Refusal {
    Missing,
    Malformed,
    Algorithm,
    Signature,
    Expired,
    NotYet,
    Issuer,
    Audience,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Missing => "missing",
            Refusal::Malformed => "malformed",
            Refusal::Algorithm => "algorithm",
            Refusal::Signature => "signature",
            Refusal::Expired => "expired",
            Refusal::NotYet => "not_yet_valid",
            Refusal::Issuer => "issuer",
            Refusal::Audience => "audience",
        }
    }
}

impl JwtVerifier {
    pub fn from_config(cfg: &JwtConfig) -> anyhow::Result<Self> {
        let key = if let Some(secret) = cfg.secret_material()? {
            Key::Hmac(secret)
        } else if let Some(path) = &cfg.public_key {
            let pem = std::fs::read(path).map_err(|e| anyhow::anyhow!("read public_key {path}: {e}"))?;
            Key::Public(PKey::public_key_from_pem(&pem).map_err(|e| anyhow::anyhow!("public_key {path}: {e}"))?)
        } else {
            anyhow::bail!("jwt needs secret, secret_file, or public_key");
        };
        Ok(JwtVerifier {
            key,
            issuer: cfg.issuer.clone(),
            audience: cfg.audience.clone(),
            leeway_secs: crate::config::parse_duration(&cfg.leeway).map(|d| d.as_secs()).unwrap_or(30),
            claim_headers: cfg.claim_headers.iter().map(|(c, h)| (c.clone(), h.clone())).collect(),
        })
    }

    /// A verifier that refuses everything — used when the configured key
    /// cannot be loaded, so a broken key file fails closed.
    pub fn refusing() -> Self {
        JwtVerifier { key: Key::Hmac(Vec::new()), issuer: None, audience: None, leeway_secs: 0, claim_headers: Vec::new() }
    }

    /// Verify a compact token. On success returns the claims to forward as
    /// headers, per `claim_headers`.
    pub fn verify(&self, token: &str, now: u64) -> Result<Vec<(String, String)>, Refusal> {
        let mut parts = token.split('.');
        let (h64, p64, s64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(Refusal::Malformed),
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: serde_json::Value = serde_json::from_slice(&b64.decode(h64).map_err(|_| Refusal::Malformed)?).map_err(|_| Refusal::Malformed)?;
        let payload_bytes = b64.decode(p64).map_err(|_| Refusal::Malformed)?;
        let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).map_err(|_| Refusal::Malformed)?;
        let signature = b64.decode(s64).map_err(|_| Refusal::Malformed)?;
        let alg = header.get("alg").and_then(|a| a.as_str()).ok_or(Refusal::Malformed)?;

        let signed = &token[..h64.len() + 1 + p64.len()];
        self.check_signature(alg, signed.as_bytes(), &signature)?;

        // Times: exp required; nbf honoured when present; both with leeway.
        let exp = claims.get("exp").and_then(|v| v.as_u64()).ok_or(Refusal::Malformed)?;
        if now > exp.saturating_add(self.leeway_secs) {
            return Err(Refusal::Expired);
        }
        if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_u64()) {
            if now.saturating_add(self.leeway_secs) < nbf {
                return Err(Refusal::NotYet);
            }
        }
        if let Some(want) = &self.issuer {
            if claims.get("iss").and_then(|v| v.as_str()) != Some(want.as_str()) {
                return Err(Refusal::Issuer);
            }
        }
        if let Some(want) = &self.audience {
            let ok = match claims.get("aud") {
                Some(serde_json::Value::String(s)) => s == want,
                Some(serde_json::Value::Array(list)) => list.iter().any(|v| v.as_str() == Some(want.as_str())),
                _ => false,
            };
            if !ok {
                return Err(Refusal::Audience);
            }
        }

        let mut out = Vec::new();
        for (claim, header) in &self.claim_headers {
            let value = match claims.get(claim) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                Some(serde_json::Value::Bool(b)) => b.to_string(),
                _ => continue,
            };
            out.push((header.clone(), value));
        }
        Ok(out)
    }

    fn check_signature(&self, alg: &str, signed: &[u8], signature: &[u8]) -> Result<(), Refusal> {
        let digest = match alg {
            "HS256" | "RS256" | "ES256" => MessageDigest::sha256(),
            "HS384" | "RS384" | "ES384" => MessageDigest::sha384(),
            "HS512" | "RS512" => MessageDigest::sha512(),
            _ => return Err(Refusal::Algorithm),
        };
        match (&self.key, &alg[..2]) {
            (Key::Hmac(secret), "HS") => {
                if secret.is_empty() {
                    return Err(Refusal::Signature);
                }
                let key = PKey::hmac(secret).map_err(|_| Refusal::Signature)?;
                let mut signer = openssl::sign::Signer::new(digest, &key).map_err(|_| Refusal::Signature)?;
                signer.update(signed).map_err(|_| Refusal::Signature)?;
                let expected = signer.sign_to_vec().map_err(|_| Refusal::Signature)?;
                // memcmp::eq panics on unequal lengths, and the length is the
                // client's to choose.
                if expected.len() == signature.len() && openssl::memcmp::eq(&expected, signature) {
                    Ok(())
                } else {
                    Err(Refusal::Signature)
                }
            }
            (Key::Public(pkey), "RS") if pkey.rsa().is_ok() => {
                let mut v = openssl::sign::Verifier::new(digest, pkey).map_err(|_| Refusal::Signature)?;
                v.update(signed).map_err(|_| Refusal::Signature)?;
                if v.verify(signature).unwrap_or(false) { Ok(()) } else { Err(Refusal::Signature) }
            }
            (Key::Public(pkey), "ES") if pkey.ec_key().is_ok() => {
                // JWS ES signatures are raw r||s; OpenSSL wants DER.
                let half = match alg {
                    "ES256" => 32,
                    "ES384" => 48,
                    _ => return Err(Refusal::Algorithm),
                };
                if signature.len() != half * 2 {
                    return Err(Refusal::Signature);
                }
                let r = openssl::bn::BigNum::from_slice(&signature[..half]).map_err(|_| Refusal::Signature)?;
                let s = openssl::bn::BigNum::from_slice(&signature[half..]).map_err(|_| Refusal::Signature)?;
                let der = openssl::ecdsa::EcdsaSig::from_private_components(r, s)
                    .and_then(|sig| sig.to_der())
                    .map_err(|_| Refusal::Signature)?;
                let mut v = openssl::sign::Verifier::new(digest, pkey).map_err(|_| Refusal::Signature)?;
                v.update(signed).map_err(|_| Refusal::Signature)?;
                if v.verify(&der).unwrap_or(false) { Ok(()) } else { Err(Refusal::Signature) }
            }
            _ => Err(Refusal::Algorithm),
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The bearer token from an `Authorization: Bearer …` style header value.
pub fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    fn token(alg: &str, claims: &str, sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
        let signed = format!("{}.{}", b64(format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#).as_bytes()), b64(claims.as_bytes()));
        let sig = sign(signed.as_bytes());
        format!("{signed}.{}", b64(&sig))
    }

    fn hmac(secret: &[u8], md: MessageDigest) -> impl Fn(&[u8]) -> Vec<u8> + '_ {
        move |data| {
            let key = PKey::hmac(secret).unwrap();
            let mut s = openssl::sign::Signer::new(md, &key).unwrap();
            s.update(data).unwrap();
            s.sign_to_vec().unwrap()
        }
    }

    fn cfg_secret(secret: &str) -> JwtConfig {
        JwtConfig {
            secret: Some(base64::engine::general_purpose::STANDARD.encode(secret)),
            secret_file: None,
            public_key: None,
            issuer: Some("https://issuer.test".into()),
            audience: Some("api".into()),
            header: "Authorization".into(),
            leeway: "30s".into(),
            claim_headers: BTreeMap::from([("sub".to_owned(), "X-Auth-Subject".to_owned())]),
        }
    }

    #[test]
    fn hs256_accepts_valid_and_refuses_each_failure() {
        let v = JwtVerifier::from_config(&cfg_secret("topsecret")).unwrap();
        let sign = hmac(b"topsecret", MessageDigest::sha256());
        let now = 1_700_000_000;
        let good = token("HS256", r#"{"sub":"alice","iss":"https://issuer.test","aud":["web","api"],"exp":1700000600,"nbf":1699999000}"#, &sign);
        assert_eq!(v.verify(&good, now).unwrap(), vec![("X-Auth-Subject".to_owned(), "alice".to_owned())]);

        let wrong = hmac(b"other", MessageDigest::sha256());
        let bad_sig = token("HS256", r#"{"sub":"a","iss":"https://issuer.test","aud":"api","exp":1700000600}"#, &wrong);
        assert_eq!(v.verify(&bad_sig, now).unwrap_err(), Refusal::Signature);
        let expired = token("HS256", r#"{"iss":"https://issuer.test","aud":"api","exp":1699999000}"#, &sign);
        assert_eq!(v.verify(&expired, now).unwrap_err(), Refusal::Expired);
        let within_leeway = token("HS256", r#"{"iss":"https://issuer.test","aud":"api","exp":1699999980}"#, &sign);
        assert!(v.verify(&within_leeway, now).is_ok());
        let early = token("HS256", r#"{"iss":"https://issuer.test","aud":"api","exp":1700000600,"nbf":1700000100}"#, &sign);
        assert_eq!(v.verify(&early, now).unwrap_err(), Refusal::NotYet);
        let iss = token("HS256", r#"{"iss":"https://evil.test","aud":"api","exp":1700000600}"#, &sign);
        assert_eq!(v.verify(&iss, now).unwrap_err(), Refusal::Issuer);
        let aud = token("HS256", r#"{"iss":"https://issuer.test","aud":"other","exp":1700000600}"#, &sign);
        assert_eq!(v.verify(&aud, now).unwrap_err(), Refusal::Audience);
        let no_exp = token("HS256", r#"{"iss":"https://issuer.test","aud":"api"}"#, &sign);
        assert_eq!(v.verify(&no_exp, now).unwrap_err(), Refusal::Malformed);
        assert_eq!(v.verify("not.a.jwt.at.all", now).unwrap_err(), Refusal::Malformed);
        assert_eq!(v.verify("garbage", now).unwrap_err(), Refusal::Malformed);
        // alg none and a public-key alg against a secret are refused before any check.
        let none = format!("{}.{}.", b64(br#"{"alg":"none"}"#), b64(br#"{"exp":1700000600}"#));
        assert_eq!(v.verify(&none, now).unwrap_err(), Refusal::Algorithm);
        let rs = token("RS256", r#"{"exp":1700000600}"#, &sign);
        assert_eq!(v.verify(&rs, now).unwrap_err(), Refusal::Algorithm);
    }

    #[test]
    fn hs_signature_of_the_wrong_length_is_refused() {
        let v = JwtVerifier::from_config(&cfg_secret("topsecret")).unwrap();
        let claims = r#"{"iss":"https://issuer.test","aud":"api","exp":1700000600}"#;
        for len in [0usize, 1, 31, 33, 64] {
            let t = token("HS256", claims, move |_| vec![0u8; len]);
            assert_eq!(v.verify(&t, 1_700_000_000).unwrap_err(), Refusal::Signature, "{len}-byte signature");
        }
    }

    #[test]
    fn rs256_and_es256_with_generated_keys() {
        let dir = std::env::temp_dir().join(format!("keel-jwt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let rsa_key = PKey::from_rsa(rsa).unwrap();
        let rsa_pub = dir.join("rsa.pem");
        std::fs::write(&rsa_pub, rsa_key.public_key_to_pem().unwrap()).unwrap();
        let mut cfg = cfg_secret("unused");
        cfg.secret = None;
        cfg.public_key = Some(rsa_pub.to_string_lossy().into_owned());
        cfg.issuer = None;
        cfg.audience = None;
        let v = JwtVerifier::from_config(&cfg).unwrap();
        let sign_rs = |data: &[u8]| {
            let mut s = openssl::sign::Signer::new(MessageDigest::sha256(), &rsa_key).unwrap();
            s.update(data).unwrap();
            s.sign_to_vec().unwrap()
        };
        let good = token("RS256", r#"{"sub":"bob","exp":1700000600}"#, sign_rs);
        assert_eq!(v.verify(&good, 1_700_000_000).unwrap(), vec![("X-Auth-Subject".to_owned(), "bob".to_owned())]);
        // A HS token "signed" with the public key bytes must not pass (alg confusion).
        let hs = token("HS256", r#"{"exp":1700000600}"#, hmac(rsa_key.public_key_to_pem().unwrap().as_slice(), MessageDigest::sha256()));
        assert_eq!(v.verify(&hs, 1_700_000_000).unwrap_err(), Refusal::Algorithm);

        let group = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        let ec = openssl::ec::EcKey::generate(&group).unwrap();
        let ec_key = PKey::from_ec_key(ec).unwrap();
        let ec_pub = dir.join("ec.pem");
        std::fs::write(&ec_pub, ec_key.public_key_to_pem().unwrap()).unwrap();
        cfg.public_key = Some(ec_pub.to_string_lossy().into_owned());
        let v = JwtVerifier::from_config(&cfg).unwrap();
        let sign_es = |data: &[u8]| {
            // Sign DER, then convert to the raw r||s form JWS uses.
            let mut s = openssl::sign::Signer::new(MessageDigest::sha256(), &ec_key).unwrap();
            s.update(data).unwrap();
            let der = s.sign_to_vec().unwrap();
            let sig = openssl::ecdsa::EcdsaSig::from_der(&der).unwrap();
            let mut raw = vec![0u8; 64];
            let r = sig.r().to_vec();
            let s_ = sig.s().to_vec();
            raw[32 - r.len()..32].copy_from_slice(&r);
            raw[64 - s_.len()..].copy_from_slice(&s_);
            raw
        };
        let good = token("ES256", r#"{"sub":"carol","exp":1700000600}"#, sign_es);
        assert_eq!(v.verify(&good, 1_700_000_000).unwrap(), vec![("X-Auth-Subject".to_owned(), "carol".to_owned())]);
        let tampered = good[..good.len() - 2].to_owned() + "AA";
        assert_eq!(v.verify(&tampered, 1_700_000_000).unwrap_err(), Refusal::Signature);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bearer_extraction() {
        assert_eq!(bearer_token("Bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(bearer_token("bearer   x"), Some("x"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }
}
