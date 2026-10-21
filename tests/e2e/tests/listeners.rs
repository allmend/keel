//! Named listeners: `addresses` is the same on every node, and each node
//! binds those it has. Status and `keel_listener_bound` show what was bound
//! and what is not on this node.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.96.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.96.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80", "10790"]
    networks: { net: { ipv4_address: 172.29.96.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}

metrics {
  address = "0.0.0.0:10790"
}
"#;

// .10 is this node; .11 and .12 would be other nodes'.
const CONFIG: &str = r#"
listener "web" {
  protocol  = "http"
  addresses = ["172.29.96.10", "172.29.96.11"]
  port      = 80
}

listener "edge" {
  protocol  = "http"
  addresses = ["172.29.96.12"]
  port      = 8080
}

pool "web" {
  backend "172.29.96.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn each_node_binds_the_addresses_it_has() -> Result<()> {
    let stack = Stack::up("listeners", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    let out = stack.exec("keel", &["keel", "-o", "json", "status"])?;
    let status: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        status["listeners"],
        serde_json::json!([
            { "name": "web", "protocol": "http", "bound": ["172.29.96.10:80"], "skipped": ["172.29.96.11:80"] },
            { "name": "edge", "protocol": "http", "bound": [], "skipped": ["172.29.96.12:8080"] },
        ])
    );
    let text = String::from_utf8_lossy(&stack.exec("keel", &["keel", "status"])?.stdout).into_owned();
    assert!(text.contains("listener web (http): 172.29.96.10:80; not on this node: 172.29.96.11:80"), "{text}");
    assert!(text.contains("listener edge (http): none of 172.29.96.12:8080 on this node"), "{text}");

    let (_, metrics) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    for line in [
        r#"keel_listener_bound{listener="web",address="172.29.96.10:80"} 1"#,
        r#"keel_listener_bound{listener="web",address="172.29.96.11:80"} 0"#,
        r#"keel_listener_bound{listener="edge",address="172.29.96.12:8080"} 0"#,
    ] {
        assert!(metrics.contains(line), "{line} in:\n{metrics}");
    }
    assert!(stack.logs("keel")?.contains("master: addresses not on this node"));
    Ok(())
}
