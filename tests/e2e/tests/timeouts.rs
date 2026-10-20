//! Timeouts and limits: upstream connect and read timeouts, one retry on
//! another backend, client keepalive and body timeouts, header and body
//! size limits, and the TCP idle timeout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.94.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.94.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80", "6000", "10790"]
    networks: { net: { ipv4_address: 172.29.94.10 } }
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

// 172.29.94.99 is in the subnet but answers nothing: connects time out.
const CONFIG: &str = r#"
keel {
  keepalive_timeout = "2s"
  body_timeout      = "2s"
  max_header_size   = "2K"
}

listener "http" "0.0.0.0:80" {}

listener "tcp" "0.0.0.0:6000" {
  pool         = "web"
  idle_timeout = "2s"
}

access_log {
  dir = "-"
}

pool "web" {
  backend "172.29.94.20:80" {}
}

pool "slow" {
  read_timeout = "1s"

  backend "172.29.94.20:80" {}
}

pool "dead" {
  connect_timeout = "1s"

  backend "172.29.94.99:80" {}
}

pool "half" {
  algorithm       = "round_robin"
  connect_timeout = "1s"

  passive {
    enabled = false
  }

  backend "172.29.94.99:80" {}
  backend "172.29.94.20:80" {}
}

vhost "web.test" {
  pool          = "web"
  max_body_size = "1K"
}

vhost "slow.test" {
  pool = "slow"
}

vhost "dead.test" {
  pool = "dead"
}

vhost "half.test" {
  pool = "half"
}
"#;

/// Send `request` raw and read until the connection closes or 10s pass.
/// The response's status code, 0 when nothing came, and how long it took.
fn raw(addr: SocketAddr, request: &[u8]) -> Result<(u16, Duration)> {
    let started = Instant::now();
    let mut sock = TcpStream::connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    sock.write_all(request)?;
    let mut out = Vec::new();
    let _ = sock.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((status, started.elapsed()))
}

fn sample(stack: &Stack, series: &str) -> Result<f64> {
    let (_, body) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    Ok(body.lines().find(|l| l.starts_with(series)).and_then(|l| l.rsplit(' ').next()?.parse().ok()).unwrap_or(0.0))
}

#[test]
#[ignore = "needs Docker"]
fn timeouts_retries_and_limits() -> Result<()> {
    let stack = Stack::up("timeouts", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "web.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // Connect timeout: 504 after about a second, not the kernel's minutes.
    let started = Instant::now();
    assert_eq!(http_get(addr, "dead.test", "/")?.0, 504);
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());

    // Read timeout: the backend answers after 3s, the pool waits 1s.
    let started = Instant::now();
    assert_eq!(http_get(addr, "slow.test", "/?wait=3s")?.0, 504);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());

    // Half the pool is dead: every request still gets through, once retried.
    for _ in 0..6 {
        assert_eq!(http_get(addr, "half.test", "/")?.0, 200);
    }
    assert!(sample(&stack, r#"keel_upstream_retries_total{pool="half"}"#)? >= 1.0);

    // Header and body limits.
    let big = format!("GET / HTTP/1.1\r\nHost: web.test\r\nX-Big: {}\r\nConnection: close\r\n\r\n", "x".repeat(3000));
    assert_eq!(raw(addr, big.as_bytes())?.0, 431);
    let body = "x".repeat(2000);
    let post = format!("POST / HTTP/1.1\r\nHost: web.test\r\nContent-Length: 2000\r\nConnection: close\r\n\r\n{body}");
    assert_eq!(raw(addr, post.as_bytes())?.0, 413);
    let chunked = format!("POST / HTTP/1.1\r\nHost: web.test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7d0\r\n{body}\r\n0\r\n\r\n");
    assert_eq!(raw(addr, chunked.as_bytes())?.0, 413);
    let small = "POST / HTTP/1.1\r\nHost: web.test\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    assert_eq!(raw(addr, small.as_bytes())?.0, 200);

    // Body timeout: a body that stops coming ends the request.
    let (_, took) = raw(addr, b"POST / HTTP/1.1\r\nHost: web.test\r\nContent-Length: 100\r\n\r\nonly a little")?;
    assert!(took >= Duration::from_secs(2) && took < Duration::from_secs(6), "{took:?}");

    // Keepalive: an idle client connection is closed after 2s.
    let (status, took) = raw(addr, b"GET / HTTP/1.1\r\nHost: web.test\r\n\r\n")?;
    assert_eq!(status, 200);
    assert!(took >= Duration::from_secs(2) && took < Duration::from_secs(6), "{took:?}");

    // TCP idle timeout: nothing either way for 2s closes the connection.
    let (status, took) = raw(stack.addr("keel", 6000)?, b"")?;
    assert_eq!(status, 0);
    assert!(took >= Duration::from_secs(2) && took < Duration::from_secs(6), "{took:?}");

    let logs = stack.logs("keel")?;
    for (vhost, error) in [("dead.test", "upstream_connect"), ("slow.test", "upstream_timeout"), ("web.test", "header_too_large"), ("web.test", "body_too_large")] {
        let found = logs.lines().any(|l| l.contains(&format!(r#""vhost":"{vhost}""#)) && l.contains(&format!(r#""error":"{error}""#)));
        assert!(found, "{vhost} {error} in:\n{logs}");
    }
    assert!(logs.lines().any(|l| l.contains(r#""type":"tcp""#) && l.contains(r#""error":"idle_timeout""#)), "{logs}");
    Ok(())
}
