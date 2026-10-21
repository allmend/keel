//! Access log files: requests Keel answers itself are logged under their
//! vhost, and a file renamed by logrotate is reopened at its path.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.97.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.97.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.97.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 1
  user    = "nobody"
  group   = "nogroup"
}
"#;

const CONFIG: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "web" {
  backend "172.29.97.20:80" {}
}

vhost "web.test" {
  pool = "web"
}

vhost "secure.test" {
  pool          = "web"
  redirect_http = true

  tls {
    cert = "certs/secure.crt"
    key  = "certs/secure.key"
  }
}

vhost "*" {
  default_action {
    status = 404
    body   = "no such site"
  }
}
"#;

/// The JSON lines of an access log file in the container.
fn entries(stack: &Stack, file: &str) -> Result<Vec<serde_json::Value>> {
    let out = stack.exec("keel", &["cat", &format!("/var/log/keel/{file}")])?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

#[test]
#[ignore = "needs Docker"]
fn self_answered_requests_and_rotated_files() -> Result<()> {
    let key = rcgen::KeyPair::generate()?;
    let cert = rcgen::CertificateParams::new(vec!["secure.test".to_owned()])?.self_signed(&key)?;
    let stack = Stack::up(
        "access-log",
        COMPOSE,
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", CONFIG.to_owned()),
            ("config/certs/secure.crt", cert.pem()),
            ("config/certs/secure.key", key.serialize_pem()),
        ],
    )?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "web.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // Answered by Keel: logged under the vhost, with method and URI, no error.
    assert_eq!(http_get(addr, "secure.test", "/login?next=1")?.0, 301);
    assert_eq!(http_get(addr, "unknown.test", "/x")?.0, 404);
    let redirect = wait_until("the redirect logged", Duration::from_secs(5), || Ok(entries(&stack, "access_secure.test.log")?.pop()))?;
    assert_eq!((redirect["status"].as_u64(), redirect["method"].as_str(), redirect["uri"].as_str()), (Some(301), Some("GET"), Some("/login?next=1")));
    assert!(redirect["error"].is_null() && redirect["backend_addr"].is_null(), "{redirect}");
    let default = entries(&stack, "access_*.log")?.pop().expect("the default action logged");
    assert_eq!((default["status"].as_u64(), default["uri"].as_str(), default["vhost"].as_str()), (Some(404), Some("/x"), Some("*")));
    assert!(default["error"].is_null(), "{default}");
    assert!(entries(&stack, "error_*.log")?.is_empty() && entries(&stack, "error_unknown.log")?.is_empty());

    // logrotate's default: rename, and the next writes go to a new file.
    let out = stack.exec("keel", &["mv", "/var/log/keel/access_web.test.log", "/var/log/keel/access_web.test.log.1"])?;
    assert!(out.status.success());
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..3 {
        assert_eq!(http_get(addr, "web.test", "/after")?.0, 200);
    }
    let fresh = wait_until("a new file", Duration::from_secs(5), || {
        Ok(Some(entries(&stack, "access_web.test.log")?).filter(|e| !e.is_empty()))
    })?;
    assert!(fresh.iter().all(|e| e["uri"] == "/after"), "{fresh:?}");
    assert!(entries(&stack, "access_web.test.log.1")?.iter().all(|e| e["uri"] == "/"));
    Ok(())
}
