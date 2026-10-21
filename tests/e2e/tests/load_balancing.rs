//! Backend selection: a pool weighted 1:3 splits traffic about 1:3, not
//! evenly, and a pool with no available backend answers 503, while a backend
//! that cannot be reached answers 502.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.84.0/24
services:
  light:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.84.21 } }
  heavy:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.84.22 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.84.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
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

access_log {
  dir = "-"
}

pool "web" {
  algorithm = "round_robin"

  backend "172.29.84.21:80" {}
  backend "172.29.84.22:80" { weight = 3 }
}

vhost "*" {
  pool = "web"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn a_pool_weighted_one_to_three_splits_traffic_one_to_three() -> Result<()> {
    let stack = Stack::up("weights", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // whoami echoes the address it answered on.
    const REQUESTS: usize = 400;
    let mut light = 0;
    for _ in 0..REQUESTS {
        let (status, body) = http_get(addr, "e2e.test", "/")?;
        assert_eq!(status, 200);
        if body.contains("IP: 172.29.84.21") {
            light += 1;
        }
    }
    // Expected 100 of 400; an unweighted pool gives about 200.
    assert!((80..=120).contains(&light), "light backend served {light} of {REQUESTS}, expected about 100");

    // The access log on stdout, one JSON entry per request, typed.
    let logs = stack.logs("keel")?;
    let entry = logs.lines().find(|l| l.starts_with('{')).expect("an access entry on stdout");
    assert!(entry.contains(r#""type":"http""#), "{entry}");
    Ok(())
}

const EMPTY_COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.86.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.86.21 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.86.10 } }
"#;

// Nothing listens on port 81. `checked` probes it and goes unhealthy;
// `unchecked` has no health check, so Keel tries the backend and fails.
const EMPTY_CONFIG: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "checked" {
  health_check "tcp" {
    interval            = "1s"
    unhealthy_threshold = 1
  }

  backend "172.29.86.21:81" {}
}

pool "unchecked" {
  backend "172.29.86.21:81" {}
}

vhost "checked.test" {
  pool = "checked"
}

vhost "unchecked.test" {
  pool = "unchecked"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn a_pool_without_available_backends_answers_503_and_a_failed_upstream_502() -> Result<()> {
    let stack = Stack::up("empty-pool", EMPTY_COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", EMPTY_CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("503 from the unhealthy pool", Duration::from_secs(60), || {
        Ok(http_get(addr, "checked.test", "/").ok().filter(|(status, _)| *status == 503))
    })?;
    for _ in 0..10 {
        assert_eq!(http_get(addr, "unchecked.test", "/")?.0, 502);
    }
    Ok(())
}

// Nothing listens on port 9: the pool's only backend fails its check.
const DEAD_UDP_CONFIG: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

listener "dns" {
  protocol  = "udp"
  addresses = ["0.0.0.0"]
  port      = 5353
  pool      = "dead"
}

access_log {
  dir = "-"
}

pool "web" {
  backend "172.29.86.21:80" {}
}

pool "dead" {
  health_check "tcp" {
    port                = 9
    interval            = "1s"
    unhealthy_threshold = 1
  }

  backend "172.29.86.21:53" {}
}

vhost "*" {
  pool = "web"
}
"#;

#[test]
#[ignore = "needs Docker"]
fn a_udp_flood_against_a_dead_pool_logs_a_line_per_second() -> Result<()> {
    let stack = Stack::up("udp-dead", EMPTY_COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", DEAD_UDP_CONFIG.to_owned())])?;
    wait_until("the dead pool unhealthy", Duration::from_secs(60), || {
        let out = stack.exec("keel", &["keel", "backend", "list", "--pool", "dead"])?;
        Ok(String::from_utf8_lossy(&out.stdout).contains("unhealthy").then_some(()))
    })?;
    // 100 datagrams every 50 ms: a burst would overflow the socket buffer.
    let flood = "for i in $(seq 20); do for j in $(seq 100); do printf x > /dev/udp/127.0.0.1/5353; done; sleep 0.05; done";
    let out = stack.exec("keel", &["bash", "-c", flood])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::thread::sleep(Duration::from_secs(2));

    let logs = stack.logs("keel")?;
    let lines: Vec<&str> = logs.lines().filter(|l| l.contains(r#""error":"no_backend""#) && l.contains(r#""type":"udp""#)).collect();
    let dropped: u64 = lines
        .iter()
        .map(|l| l.split(r#""packets_in":"#).nth(1).and_then(|r| r.split(',').next()).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0))
        .sum();
    assert_eq!(dropped, 2000, "every dropped datagram is in the log");
    assert!(lines.len() <= 20, "{} lines for 2000 datagrams", lines.len());
    Ok(())
}
