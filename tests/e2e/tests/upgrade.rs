//! keel upgrade: the master runs the binary on disk in place. Same PID, new
//! workers, open connections finish on the old ones, counters carry on. A
//! binary that does not run is refused before anything changes.

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
        - subnet: 172.29.104.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.104.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80", "6000", "10790"]
    networks: { net: { ipv4_address: 172.29.104.10 } }
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

const CONFIG: &str = r#"
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
  backend "172.29.104.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

const REQUESTS: &str = r#"keel_requests_total{pool="web",vhost="*",status="200"}"#;

fn sample(stack: &Stack, series: &str) -> Result<f64> {
    let (_, body) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    Ok(body.lines().find(|l| l.starts_with(series)).and_then(|l| l.rsplit(' ').next()?.parse().ok()).unwrap_or(-1.0))
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

/// (master pid, worker pids): the master is the keel process whose parent
/// is not keel.
fn processes(stack: &Stack) -> Result<(u32, Vec<u32>)> {
    let procs = stack.procs("keel")?;
    let keel: Vec<_> = procs.iter().filter(|p| p.cmd.starts_with("/usr/local/bin/keel")).collect();
    let master = keel.iter().find(|p| !keel.iter().any(|q| q.pid == p.ppid)).map(|p| p.pid).unwrap_or_default();
    let mut workers: Vec<u32> = keel.iter().filter(|p| p.ppid == master && p.uid == 65534).map(|p| p.pid).collect();
    workers.sort();
    Ok((master, workers))
}

#[test]
#[ignore = "needs Docker"]
fn keel_upgrade_runs_the_new_binary_in_place() -> Result<()> {
    let stack = Stack::up("upgrade", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    for _ in 0..5 {
        assert_eq!(http_get(addr, "e2e.test", "/")?.0, 200);
    }
    wait_until("requests counted", Duration::from_secs(10), || Ok((sample(&stack, REQUESTS)? >= 6.0).then_some(())))?;
    let counted = sample(&stack, REQUESTS)?;
    let (master, workers) = processes(&stack)?;

    // A binary that does not run is refused; nothing changes.
    // Replaced the way packages do it: a new file renamed over the old one.
    let swap = "cp /usr/local/bin/keel /keel.real && printf '#!/bin/sh\\nexit 1\\n' > /tmp/broken && chmod +x /tmp/broken && mv /tmp/broken /usr/local/bin/keel";
    assert!(stack.exec("keel", &["sh", "-c", swap])?.status.success());
    let out = stack.exec("keel", &["/keel.real", "upgrade"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("the refusal", Duration::from_secs(10), || {
        Ok(stack.logs("keel")?.contains("master: upgrade refused").then_some(()))
    })?;
    assert!(stack.exec("keel", &["sh", "-c", "mv /keel.real /usr/local/bin/keel"])?.status.success());
    assert_eq!(processes(&stack)?, (master, workers.clone()), "nothing changed");

    // Through the old workers: a held TCP connection and a slow request.
    let mut held = TcpStream::connect(stack.addr("keel", 6000)?)?;
    held.set_read_timeout(Some(Duration::from_secs(10)))?;
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK");
    let slow = std::thread::spawn(move || http_get(addr, "e2e.test", "/?wait=3s").map(|(status, _)| status));
    std::thread::sleep(Duration::from_millis(300));

    let out = stack.exec("keel", &["keel", "upgrade"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_until("the upgrade", Duration::from_secs(60), || {
        Ok(stack.logs("keel")?.contains("master: upgrade complete").then_some(()))
    })?;
    let (master_after, workers_after) = processes(&stack)?;
    assert_eq!(master_after, master, "the same master process");
    // The old workers still finish what they hold.
    let new: Vec<u32> = workers_after.iter().copied().filter(|w| !workers.contains(w)).collect();
    assert_eq!(new.len(), 2, "two new workers: {workers:?} → {workers_after:?}");

    assert_eq!(slow.join().expect("the slow request")?, 200);
    assert_eq!(ask(&mut held)?, "HTTP/1.1 200 OK", "the held connection survived");
    assert_eq!(http_get(addr, "e2e.test", "/")?.0, 200);
    assert!(sample(&stack, REQUESTS)? > counted, "counters carried on");
    assert!(sample(&stack, "keel_build_info{")? == 1.0);

    drop(held);
    wait_until("old workers gone", Duration::from_secs(15), || Ok((sample(&stack, "keel_workers_draining ")? == 0.0).then_some(())))?;
    assert_eq!(processes(&stack)?.1, new, "only the new workers are left");
    Ok(())
}
