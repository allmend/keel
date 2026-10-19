//! The configuration language: a node configured in HCL, with locals shared
//! across the config directory, serves traffic; `keel config validate` names
//! the file, line and column of a mistake; `keel config fmt` finds files that
//! are not formatted and leaves them alone when told not to write.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.88.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.88.21 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes:
      - ./node.hcl:/etc/keel/node.hcl:ro
      - ./config:/etc/keel/config:ro
      - ./broken:/broken:ro
      - ./messy:/messy:ro
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.88.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

// The local is defined in keel.hcl and used in another file.
const ROOT: &str = r#"
locals {
  backend_port = 80
  check_every  = "1s"
}

listener "http" "0.0.0.0:80" {}

vhost "*" {
  pool = "web"
}
"#;

const POOL: &str = r#"
pool "web" {
  health_check "http" {
    path     = "/health"
    interval = local.check_every
  }

  backend "172.29.88.21:80" {}
}
"#;

// Line 4, column 5: an attribute a pool does not have.
const BROKEN: &str = r#"pool "web" {
  backend "172.29.88.21:80" {}

    algorithmn = "round_robin"
}

listener "http" "0.0.0.0:80" {}
"#;

const MESSY: &str = "pool \"web\" {\nalgorithm=\"round_robin\"\n  backend \"172.29.88.21:80\" { weight=2 }\n}\n";

#[test]
#[ignore = "needs Docker"]
fn a_node_configured_in_hcl_serves_traffic_and_its_tools_check_files() -> Result<()> {
    let stack = Stack::up(
        "config-format",
        COMPOSE,
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", ROOT.to_owned()),
            ("config/pools/web.hcl", POOL.to_owned()),
            ("broken/keel.hcl", BROKEN.to_owned()),
            ("messy/keel.hcl", MESSY.to_owned()),
        ],
    )?;
    let addr = stack.addr("keel", 80)?;
    let (_, body) = wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    assert!(body.contains("IP: 172.29.88.21"), "{body}");

    // The running config validates.
    let ok = stack.exec("keel", &["keel", "config", "validate"])?;
    assert!(ok.status.success(), "validate: {}", String::from_utf8_lossy(&ok.stderr));

    // A mistake is reported where it is.
    let bad = stack.exec("keel", &["keel", "config", "validate", "/broken"])?;
    let err = String::from_utf8_lossy(&bad.stderr);
    assert_eq!(bad.status.code(), Some(1), "{err}");
    assert!(err.contains("keel.hcl:4:5"), "{err}");
    assert!(err.contains("algorithmn"), "{err}");

    // fmt --check reports the file and a diff, and writes nothing.
    let check = stack.exec("keel", &["keel", "config", "fmt", "--check", "--diff", "/messy"])?;
    let out = String::from_utf8_lossy(&check.stdout);
    assert_ne!(check.status.code(), Some(0), "{out}");
    assert!(out.contains("keel.hcl"), "{out}");
    assert!(out.contains("+  algorithm = \"round_robin\""), "{out}");
    assert!(out.contains("+  backend \"172.29.88.21:80\" { weight = 2 }"), "{out}");

    // Formatted files pass.
    let clean = stack.exec("keel", &["keel", "config", "fmt", "--check"])?;
    assert!(clean.status.success(), "{}", String::from_utf8_lossy(&clean.stdout));
    Ok(())
}
