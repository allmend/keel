//! node.hcl applies on reload: a new worker count by new workers, new
//! metrics and control addresses by a new control worker; what needs a new
//! master is refused with the reason. Traffic is never cut.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.103.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.103.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node:/etc/keel/node:ro", "./config:/etc/keel/config:ro"]
    command: ["--config", "/etc/keel/node/node.hcl"]
    ports: ["6000", "10790", "10791"]
    networks: { net: { ipv4_address: 172.29.103.10 } }
"#;

fn node(workers: usize, metrics: u16, user: &str) -> String {
    format!(
        "keel {{\n  workers    = {workers}\n  user       = \"{user}\"\n  group      = \"nogroup\"\n  config_dir = \"/etc/keel/config\"\n}}\n\nmetrics {{\n  address = \"0.0.0.0:{metrics}\"\n}}\n"
    )
}

const CONFIG: &str = r#"
listener "tcp" {
  protocol  = "tcp"
  addresses = ["0.0.0.0"]
  port      = 6000
  pool      = "web"
}

pool "web" {
  backend "172.29.103.20:80" {}
}
"#;

fn sample(stack: &Stack, port: u16, name: &str) -> Result<f64> {
    let (_, body) = http_get(stack.addr("keel", port)?, "metrics", "/metrics")?;
    Ok(body.lines().find(|l| l.starts_with(&format!("{name} "))).and_then(|l| l.rsplit(' ').next()?.parse().ok()).unwrap_or(-1.0))
}

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
fn node_hcl_applies_on_reload() -> Result<()> {
    let stack = Stack::up("node-reload", COMPOSE, &[("node/node.hcl", node(2, 10790, "nobody")), ("config/keel.hcl", CONFIG.to_owned())])?;
    wait_until("metrics", Duration::from_secs(60), || Ok((sample(&stack, 10790, "keel_workers").ok() == Some(2.0)).then_some(())))?;
    let mut held = TcpStream::connect(stack.addr("keel", 6000)?)?;
    held.set_read_timeout(Some(Duration::from_secs(10)))?;
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK");

    // Three workers, metrics on another port.
    stack.write_node_file("node/node.hcl", &node(3, 10791, "nobody"))?;
    let out = stack.exec("keel", &["keel", "config", "reload"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("three workers", Duration::from_secs(30), || Ok((sample(&stack, 10791, "keel_workers").ok() == Some(3.0)).then_some(())))?;
    assert_eq!(sample(&stack, 10791, "keel_workers_configured")?, 3.0);
    assert!(http_get(stack.addr("keel", 10790)?, "metrics", "/metrics").is_err(), "the old metrics port is closed");
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK", "the held connection survived");

    // A new process user needs a new master: refused, nothing changes.
    stack.write_node_file("node/node.hcl", &node(3, 10791, "daemon"))?;
    let out = stack.exec("keel", &["keel", "config", "reload"])?;
    assert!(out.status.success());
    wait_until("the refusal", Duration::from_secs(10), || {
        Ok(stack.logs("keel")?.contains("these need a new master (keel upgrade)").then_some(()))
    })?;
    assert!(stack.logs("keel")?.contains("settings=\"keel.user\""));
    assert_eq!(sample(&stack, 10791, "keel_workers")?, 3.0);
    Ok(())
}
