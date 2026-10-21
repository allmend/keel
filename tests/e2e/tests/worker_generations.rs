//! A version workers cannot apply in place (a new listener, the keel block)
//! starts a new generation of workers; the replaced ones stop accepting and
//! finish their connections. Nothing in flight is cut.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.101.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.101.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro", "./next:/next:ro"]
    ports: ["80", "6000", "8080", "10790"]
    networks: { net: { ipv4_address: 172.29.101.10 } }
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

const BEFORE: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

listener "tcp" {
  protocol  = "tcp"
  addresses = ["0.0.0.0"]
  port      = 6000
  pool      = "web"
}

pool "web" {
  backend "172.29.101.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

/// Workers of the current generation, as the master counts them.
fn sample(stack: &Stack, name: &str) -> Result<f64> {
    let (_, body) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    Ok(body.lines().find(|l| l.starts_with(&format!("{name} "))).and_then(|l| l.rsplit(' ').next()?.parse().ok()).unwrap_or(-1.0))
}

/// One keep-alive HTTP request on `sock`: its status line.
fn ask(sock: &mut TcpStream) -> Result<String> {
    write!(sock, "GET / HTTP/1.1\r\nHost: e2e.test\r\n\r\n")?;
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
            length = v.trim().parse()?;
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    std::io::Read::read_exact(&mut reader, &mut body)?;
    Ok(status.trim().to_owned())
}

#[test]
#[ignore = "needs Docker"]
fn a_new_listener_starts_new_workers_without_cutting_connections() -> Result<()> {
    let after = BEFORE.replace("listener \"tcp\" {", "listener \"extra\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 8080\n}\n\nlistener \"tcp\" {")
        + "\nkeel {\n  keepalive_timeout = \"30s\"\n}\n";
    let stack = Stack::up(
        "generations",
        COMPOSE,
        &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", BEFORE.to_owned()), ("next/keel.hcl", after)],
    )?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;

    // Through the old workers: a TCP connection held open, a slow request.
    let mut held = TcpStream::connect(stack.addr("keel", 6000)?)?;
    held.set_read_timeout(Some(Duration::from_secs(10)))?;
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK");
    let slow = std::thread::spawn(move || http_get(addr, "e2e.test", "/?wait=4s").map(|(status, _)| status));
    std::thread::sleep(Duration::from_millis(300));

    let out = stack.exec("keel", &["keel", "config", "push", "/next"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let started = Instant::now();

    // The new generation serves the new listener.
    let extra = stack.addr("keel", 8080)?;
    wait_until("the new listener", Duration::from_secs(30), || {
        Ok(http_get(extra, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    assert!(started.elapsed() < Duration::from_secs(30));
    // An old worker with nothing open is gone at once; the one holding the
    // TCP connection drains.
    wait_until("an old worker draining", Duration::from_secs(10), || Ok((sample(&stack, "keel_workers_draining")? >= 1.0).then_some(())))?;
    assert_eq!(sample(&stack, "keel_workers")?, 2.0);

    // Nothing in flight was cut.
    assert_eq!(slow.join().expect("the slow request")?, 200);
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK", "the held connection still works");
    assert_eq!(http_get(addr, "e2e.test", "/")?.0, 200);

    // The old workers finish once their last connection closes.
    drop(held);
    wait_until("old workers gone", Duration::from_secs(10), || Ok((sample(&stack, "keel_workers_draining")? == 0.0).then_some(())))?;
    assert_eq!(sample(&stack, "keel_worker_replacements_failed_total")?, 0.0);
    let logs = stack.logs("keel")?;
    assert!(logs.contains("master: new workers ready; the replaced ones drain"), "{logs}");
    assert!(logs.contains("worker: exiting") && logs.contains("drained=true"), "{logs}");
    Ok(())
}
