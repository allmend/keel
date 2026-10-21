//! Pools change live: a push adds backends and pools, changes weights,
//! health checks and timeouts, without new processes and without closing a
//! connection.

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
        - subnet: 172.29.99.0/24
services:
  a:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.99.20 } }
  b:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.99.21 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro", "./next:/next:ro"]
    ports: ["80"]
    networks: { net: { ipv4_address: 172.29.99.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}
"#;

const BEFORE: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "web" {
  health_check "http" {
    path     = "/health"
    interval = "1s"
  }

  backend "172.29.99.20:80" {}
}

pool "checked" {
  backend "172.29.99.20:80" {}
}

vhost "web.test" {
  pool = "web"
}

vhost "checked.test" {
  pool = "checked"
}
"#;

// b added to web with weight 3, read_timeout down to 1s; checked gets a
// check it fails; a new pool and vhost.
const AFTER: &str = r#"
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}

pool "web" {
  read_timeout = "1s"

  health_check "http" {
    path     = "/health"
    interval = "1s"
  }

  backend "172.29.99.20:80" {}
  backend "172.29.99.21:80" { weight = 3 }
}

pool "checked" {
  health_check "http" {
    path        = "/"
    interval    = "1s"
    expect_body = "never there"
  }

  backend "172.29.99.20:80" {}
}

pool "api" {
  backend "172.29.99.21:80" {}
}

vhost "web.test" {
  pool = "web"
}

vhost "checked.test" {
  pool = "checked"
}

vhost "api.test" {
  pool = "api"
}
"#;

/// The backend IP whoami reports.
fn served_by(body: &str) -> Option<&str> {
    body.lines().find_map(|l| l.strip_prefix("IP: ").filter(|ip| ip.starts_with("172.29.99.2")))
}

/// Keel's worker PIDs, from inside the container.
fn workers(stack: &Stack) -> Result<Vec<u32>> {
    let mut pids: Vec<u32> = stack.procs("keel")?.into_iter().filter(|p| p.uid == 65534 && p.cmd.starts_with("/usr/local/bin/keel")).map(|p| p.pid).collect();
    pids.sort();
    Ok(pids)
}

#[test]
#[ignore = "needs Docker"]
fn pools_change_live() -> Result<()> {
    let stack = Stack::up(
        "live-reload",
        COMPOSE,
        &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", BEFORE.to_owned()), ("next/keel.hcl", AFTER.to_owned())],
    )?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "web.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    assert_eq!(http_get(addr, "web.test", "/?wait=2s")?.0, 200, "the default read timeout is 60s");
    assert_eq!(http_get(addr, "checked.test", "/")?.0, 200);
    let before = workers(&stack)?;

    // A keep-alive connection open across the push.
    let mut held = TcpStream::connect(addr)?;
    held.set_read_timeout(Some(Duration::from_secs(5)))?;
    let ask = |held: &mut TcpStream| -> Result<String> {
        write!(held, "GET / HTTP/1.1\r\nHost: web.test\r\n\r\n")?;
        let mut reader = BufReader::new(held.try_clone()?);
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
        Ok(status)
    };
    assert!(ask(&mut held)?.starts_with("HTTP/1.1 200"));

    let out = stack.exec("keel", &["keel", "config", "push", "/next"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // The added backend takes traffic, three times as much: measured once
    // both workers have the version and b passed its first probe.
    wait_until("b in rotation", Duration::from_secs(10), || {
        let hits = (0..4).filter(|_| http_get(addr, "web.test", "/").is_ok_and(|(_, body)| served_by(&body) == Some("172.29.99.21"))).count();
        Ok((hits >= 2).then_some(()))
    })?;
    std::thread::sleep(Duration::from_millis(500));
    let mut hits = 0;
    for _ in 0..40 {
        let (_, body) = http_get(addr, "web.test", "/")?;
        hits += (served_by(&body) == Some("172.29.99.21")) as usize;
    }
    assert!((22..=38).contains(&hits), "about 30 of 40 to b: {hits}");
    // The new pool and vhost, the new timeout, the new check.
    assert_eq!(http_get(addr, "api.test", "/")?.0, 200);
    assert_eq!(http_get(addr, "web.test", "/?wait=2s")?.0, 504);
    wait_until("checked fails its new check", Duration::from_secs(15), || {
        Ok((http_get(addr, "checked.test", "/")?.0 == 503).then_some(()))
    })?;

    // Same processes, and the held connection still answers.
    assert_eq!(workers(&stack)?, before, "no worker was replaced");
    assert!(ask(&mut held)?.starts_with("HTTP/1.1 200"));
    Ok(())
}
