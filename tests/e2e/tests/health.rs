//! Health checks run once per node, in the control worker: a backend gets
//! one probe per interval whatever the worker count, every worker acts on the
//! same verdict, a replacement worker starts from it, and the workers keep it
//! while the control worker is down.

use std::time::{Duration, Instant};

use anyhow::Result;
use keel_e2e::{http_get, log_field, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.89.0/24
services:
  counter:
    image: python:3.12-alpine
    command: ["python", "-u", "-m", "http.server", "80"]
    networks: { net: { ipv4_address: 172.29.89.20 } }
  a:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.89.21 } }
  b:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.89.22 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.89.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 4
  user    = "nobody"
  group   = "nogroup"
}
"#;

// Nothing answers at 172.29.89.99.
const CONFIG: &str = r#"
listener "http" "0.0.0.0:80" {}

pool "counted" {
  health_check "http" {
    path     = "/"
    interval = "1s"
  }

  backend "172.29.89.20:80" {}
}

pool "web" {
  health_check "tcp" {
    interval            = "1s"
    unhealthy_threshold = 1
  }

  passive {
    enabled = false
  }

  backend "172.29.89.21:80" {}
  backend "172.29.89.22:80" {}
}

pool "pinged" {
  health_check "icmp" {
    interval = "1s"
    timeout  = "1s"
  }

  backend "172.29.89.99:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

fn probes(stack: &Stack) -> Result<usize> {
    Ok(stack.logs("counter")?.lines().filter(|l| l.contains("\"GET / HTTP/1.1\"")).count())
}

fn status_line(stack: &Stack, backend: &str) -> Result<String> {
    let out = stack.exec("keel", &["keel", "status"])?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(text.lines().find(|l| l.contains(backend)).unwrap_or_default().to_owned())
}

/// Requests for `window`; every one must reach the live backend.
fn all_served(stack: &Stack, window: Duration) -> Result<()> {
    let addr = stack.addr("keel", 80)?;
    let start = Instant::now();
    let mut n = 0;
    while start.elapsed() < window {
        let (status, body) = http_get(addr, "e2e.test", "/")?;
        assert_eq!(status, 200, "request {n} after {:?}: {body}", start.elapsed());
        assert!(body.contains("IP: 172.29.89.21"), "{body}");
        n += 1;
    }
    Ok(())
}

fn pid_of_worker(stack: &Stack, index: &str) -> Result<String> {
    let logs = stack.logs("keel")?;
    logs.lines()
        .filter(|l| l.contains("master: worker started") || l.contains("master: replacement worker started"))
        .filter(|l| log_field(l, "index") == Some(index))
        .filter_map(|l| log_field(l, "pid"))
        .next_back()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no start line for worker {index}"))
}

#[test]
#[ignore = "needs Docker"]
fn health_checks_run_once_per_node_and_every_worker_acts_on_them() -> Result<()> {
    let stack = Stack::up("health", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    wait_until("first probe", Duration::from_secs(60), || Ok((probes(&stack)? > 0).then_some(())))?;

    // One probe per second with four workers: about 10 in 10s, not 40.
    let before = probes(&stack)?;
    std::thread::sleep(Duration::from_secs(10));
    let probed = probes(&stack)? - before;
    assert!((7..=13).contains(&probed), "{probed} probes in 10s at a 1s interval");

    // Nothing answers ICMP at .99.
    wait_until("icmp unhealthy", Duration::from_secs(30), || {
        Ok(status_line(&stack, "172.29.89.99")?.contains("unhealthy").then_some(()))
    })?;

    // b goes away: every worker stops sending it traffic, with the reason in status.
    stack.stop("b")?;
    let line = wait_until("b unhealthy", Duration::from_secs(30), || {
        let line = status_line(&stack, "172.29.89.22")?;
        Ok(line.contains("unhealthy").then_some(line))
    })?;
    assert!(line.contains('('), "status shows why: {line}");
    all_served(&stack, Duration::from_secs(2))?;

    // A replacement worker starts from the verdict, not from all-healthy.
    let victim = pid_of_worker(&stack, "1")?;
    stack.exec("keel", &["sh", "-c", &format!("kill -9 {victim}")])?;
    all_served(&stack, Duration::from_secs(4))?;

    // The control worker down: the workers keep the last verdict.
    let control = stack.exec("keel", &["sh", "-c", "for d in /proc/[0-9]*; do grep -q '^Uid:.*'$(id -u keel-control) $d/status 2>/dev/null && echo ${d#/proc/}; done"])?;
    let control = String::from_utf8_lossy(&control.stdout).trim().to_owned();
    assert!(!control.is_empty(), "no control worker found");
    stack.exec("keel", &["sh", "-c", &format!("kill -9 {control}")])?;
    all_served(&stack, Duration::from_secs(3))?;
    Ok(())
}
