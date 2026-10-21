//! Access lists: `allow` and `deny` on vhosts, routes and `tcp` and `udp`
//! listeners. HTTP answers 403, TCP closes, UDP drops; the client is the
//! address behind trusted proxies; a push changes them live.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.92.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.92.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes:
      - ./node.hcl:/etc/keel/node.hcl:ro
      - ./config:/etc/keel/config:ro
      - ./opened:/opened:ro
    ports: ["80", "10790"]
    networks: { net: { ipv4_address: 172.29.92.10 } }
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

// Requests come from inside the keel container, from 127.0.0.1.
const CONFIG: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

listener "tcp-in" {
  protocol  = "tcp"
  addresses = ["0.0.0.0"]
  port      = 2201
  pool      = "web"
  allow     = ["127.0.0.1"]
}

listener "tcp-out" {
  protocol  = "tcp"
  addresses = ["0.0.0.0"]
  port      = 2202
  pool      = "web"
  deny      = ["127.0.0.0/8"]
}

listener "udp-in" {
  protocol  = "udp"
  addresses = ["0.0.0.0"]
  port      = 5301
  pool      = "dead"
  allow     = ["127.0.0.1"]
}

listener "udp-out" {
  protocol  = "udp"
  addresses = ["0.0.0.0"]
  port      = 5302
  pool      = "dead"
  deny      = ["127.0.0.0/8"]
}

access_log {
  dir = "-"
}

pool "web" {
  backend "172.29.92.20:80" {}
}

pool "dead" {
  backend "172.29.92.99:53" {}
}

vhost "closed.test" {
  deny = ["127.0.0.1"]

  route "/" {
    pool = "web"
  }

  route "/in/" {
    pool  = "web"
    allow = ["127.0.0.0/8"]
  }
}

vhost "proxied.test" {
  allow = ["198.51.100.0/24"]

  forwarded_headers {
    mode            = "append"
    trusted_proxies = ["127.0.0.0/8"]
  }

  route "/" {
    pool = "web"
  }
}
"#;

/// The status line of a request sent from inside the container, from
/// 127.0.0.1, to `port`; empty when the connection was closed. `headers` are
/// extra header lines, `\r\n`-terminated as printf reads them.
fn request(stack: &Stack, port: u16, host: &str, path: &str, headers: &str) -> Result<String> {
    let script = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} && printf 'GET {path} HTTP/1.1\\r\\nHost: {host}\\r\\n{headers}Connection: close\\r\\n\\r\\n' >&3 && head -1 <&3"
    );
    let out = stack.exec("keel", &["bash", "-c", &script])?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn get(stack: &Stack, host: &str, path: &str, headers: &str) -> Result<String> {
    request(stack, 80, host, path, headers)
}

#[test]
#[ignore = "needs Docker"]
fn allow_and_deny_on_vhosts_routes_and_listeners() -> Result<()> {
    let opened = CONFIG.replace("vhost \"closed.test\" {\n  deny = [\"127.0.0.1\"]\n\n", "vhost \"closed.test\" {\n");
    let stack = Stack::up(
        "access-lists",
        COMPOSE,
        &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned()), ("opened/keel.hcl", opened)],
    )?;
    // The Docker gateway, where the test connects from, is not denied.
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "closed.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // A vhost list, and a route's list in its place.
    assert_eq!(get(&stack, "closed.test", "/", "")?, "HTTP/1.1 403 Forbidden");
    assert_eq!(get(&stack, "closed.test", "/in/x", "")?, "HTTP/1.1 200 OK");

    // Behind a trusted proxy the client is the last untrusted hop.
    assert_eq!(get(&stack, "proxied.test", "/", "X-Forwarded-For: 198.51.100.7\\r\\n")?, "HTTP/1.1 200 OK");
    assert_eq!(get(&stack, "proxied.test", "/", "X-Forwarded-For: 198.51.100.7, 192.0.2.1\\r\\n")?, "HTTP/1.1 403 Forbidden");
    assert_eq!(get(&stack, "proxied.test", "/", "")?, "HTTP/1.1 403 Forbidden");

    // TCP: through, or closed before a backend is chosen.
    assert_eq!(request(&stack, 2201, "closed.test", "/", "")?, "HTTP/1.1 200 OK");
    assert_eq!(request(&stack, 2202, "closed.test", "/", "")?, "");

    // UDP: an allowed client opens a flow, a denied one is dropped.
    for port in [5301, 5302] {
        stack.exec("keel", &["bash", "-c", &format!("printf x > /dev/udp/127.0.0.1/{port}")])?;
    }
    // Drops are logged once a second.
    let logs = wait_until("the UDP drop logged", Duration::from_secs(10), || {
        let logs = stack.logs("keel")?;
        Ok(logs.contains(r#""listener":"udp-out""#).then_some(logs))
    })?;
    let denied: Vec<&str> = logs.lines().filter(|l| l.contains(r#""error":"denied""#)).collect();
    let of = |kind: &str| denied.iter().filter(|l| l.contains(&format!(r#""type":"{kind}""#))).count();
    assert_eq!((of("http"), of("tcp"), of("udp")), (3, 1, 1), "{logs}");
    let (_, metrics) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    for series in [r#"keel_lb_errors_total{pool="web",vhost="closed.test",reason="denied"} 1"#, r#"keel_tcp_errors_total{pool="web",reason="denied"} 1"#, r#"keel_udp_errors_total{pool="dead",reason="denied"} 1"#] {
        assert!(metrics.contains(series), "{series} in:\n{metrics}");
    }
    assert!(!denied.iter().any(|l| l.contains(r#""listener":"udp-in""#)), "{logs}");
    assert!(denied.iter().all(|l| l.contains(r#""client_addr":"127.0.0.1:"#)), "{logs}");

    // A push without the vhost list lets the client in, no restart.
    let out = stack.exec("keel", &["keel", "config", "push", "/opened"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("the list lifted", Duration::from_secs(10), || {
        Ok((get(&stack, "closed.test", "/", "")? == "HTTP/1.1 200 OK").then_some(()))
    })?;
    Ok(())
}
