//! ACME on a standalone node with two workers: a reload that adds, removes or
//! changes TLS vhosts takes effect for ACME at once, and later renewals keep
//! it. Pebble is the CA; it validates every challenge without contacting
//! anyone, and `PEBBLE_ACME` renews on every check (once a minute).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use keel_e2e::{pebble_root, pebble_service, served_certificate, wait_until, Runtime, Stack, PEBBLE_ACME};

fn compose() -> String {
    format!(
        r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.83.0/24
services:
  backend:
    image: traefik/whoami
    networks: {{ net: {{ ipv4_address: 172.29.83.20 }} }}
{pebble}  keel:
    image: {{image}}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config"]
    ports: ["80", "443"]
    networks: {{ net: {{ ipv4_address: 172.29.83.10 }} }}
"#,
        pebble = pebble_service("172.29.83.30")
    )
}

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

/// The config directory's `keel.hcl` with `vhosts` before the catch-all.
fn config(vhosts: &str) -> String {
    format!(
        r#"
listener "http" {{
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}}

listener "https" {{
  protocol  = "https"
  addresses = ["0.0.0.0"]
  port      = 443
}}

pool "web" {{
  backend "172.29.83.20:80" {{}}
}}
{vhosts}
vhost "*" {{
  pool = "web"
}}
{PEBBLE_ACME}"#
    )
}

fn byo(host: &str) -> String {
    format!("\nvhost \"{host}\" {{\n  pool = \"web\"\n\n  tls {{\n    cert = \"certs/{host}.crt\"\n    key  = \"certs/{host}.key\"\n  }}\n}}\n")
}

const ACME_VHOST: &str = "\nvhost \"acme.test\" {\n  pool = \"web\"\n\n  tls {\n    acme = \"test\"\n  }\n}\n";

/// A self-signed certificate for `host`: (cert PEM, key PEM, leaf DER).
fn cert(host: &str) -> Result<(String, String, Vec<u8>)> {
    let key = rcgen::KeyPair::generate()?;
    let cert = rcgen::CertificateParams::new(vec![host.to_owned()])?.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem(), cert.der().to_vec()))
}

/// What several connections are served for `sni`, so both workers answer.
fn served_by_all(addr: SocketAddr, sni: &str) -> Vec<Option<Vec<u8>>> {
    (0..8).map(|_| served_certificate(addr, sni).ok()).collect()
}

#[test]
#[ignore = "needs Docker"]
fn acme_follows_a_reload_and_renewals_keep_it() -> Result<()> {
    let (gone_crt, gone_key, _) = cert("gone.test")?;
    let (byo_crt, byo_key, byo_leaf) = cert("byo.test")?;
    let root = pebble_root()?;
    let stack = Stack::up(
        "acme-reload",
        &compose(),
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", config(&byo("gone.test"))),
            ("config/certs/gone.test.crt", gone_crt),
            ("config/certs/gone.test.key", gone_key),
            ("config/certs/pebble.minica.pem", root),
        ],
    )?;
    let addr = stack.addr("keel", 443)?;
    // As early as the master takes signals, before the node is ready: the
    // reload must wait for it, and the node must not write its first
    // version over the edits.
    wait_until("the master is up", Duration::from_secs(30), || {
        Ok(stack.logs("keel")?.contains("master: control worker started").then_some(()))
    })?;

    // No ACME host at startup; the reload adds one, adds a BYO vhost and
    // removes gone.test.
    stack.write_node_file("config/certs/byo.test.crt", &byo_crt)?;
    stack.write_node_file("config/certs/byo.test.key", &byo_key)?;
    stack.write_node_file("config/keel.hcl", &config(&format!("{}{ACME_VHOST}", byo("byo.test"))))?;
    stack.signal("keel", "HUP")?;

    let first = wait_until("acme.test issued without a restart", Duration::from_secs(120), || {
        Ok(served_certificate(addr, "acme.test").ok())
    })?;
    wait_until("acme.test renewed", Duration::from_secs(150), || {
        Ok(served_certificate(addr, "acme.test").ok().filter(|leaf| *leaf != first))
    })?;
    // Give the other worker a check to swap the renewal in too.
    std::thread::sleep(Duration::from_secs(5));

    assert!(
        served_by_all(addr, "byo.test").iter().all(|l| l.as_ref() == Some(&byo_leaf)),
        "the reloaded BYO certificate survives the renewal on every worker"
    );
    assert!(
        served_by_all(addr, "gone.test").iter().all(Option::is_none),
        "the removed vhost does not come back with the renewal"
    );
    let logs = stack.logs("keel")?;
    assert!(logs.contains("acme: managing certificate host=\"acme.test\""), "{logs}");
    Ok(())
}
