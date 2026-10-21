//! A checked backend takes no traffic before its first probe, and workers
//! serve only once every checked backend has a verdict: from the first
//! request on, a dead backend gets none.

use std::time::{Duration, Instant};

use anyhow::Result;
use keel_e2e::{http_get, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.98.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.98.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.98.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

// 172.29.98.99 answers nothing: its probe takes seconds to fail, and a
// request sent to it before then times out. No retry to hide that.
const CONFIG: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "web" {
  algorithm       = "round_robin"
  connect_timeout = "1s"
  retry           = false

  passive {
    enabled = false
  }

  health_check "tcp" {
    timeout = "5s"
  }

  backend "172.29.98.99:80" {}
  backend "172.29.98.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn no_traffic_before_the_first_probe() -> Result<()> {
    let stack = Stack::up("first-probe", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    // From the first answer on: every request reaches the live backend.
    let deadline = Instant::now() + Duration::from_secs(60);
    let first = loop {
        match http_get(addr, "e2e.test", "/") {
            Ok((status, _)) => break status,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e),
        }
    };
    let mut statuses = vec![first];
    for _ in 0..10 {
        statuses.push(http_get(addr, "e2e.test", "/")?.0);
    }
    assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
    Ok(())
}
