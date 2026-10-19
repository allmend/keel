//! One `/metrics` per node: the master binds it, the control worker serves
//! node totals from shared memory, and a killed worker's counts survive it.

use std::time::Duration;

use anyhow::Result;
use keel_e2e::{http_get, log_field, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.87.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.87.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80", "10790"]
    networks: { net: { ipv4_address: 172.29.87.10 } }
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
listener "http" "0.0.0.0:80" {}

pool "web" {
  backend "172.29.87.20:80" {}
}

vhost "*" {
  pool = "web"
}
"#;

/// The value of the first sample line starting with `series`.
fn sample(stack: &Stack, series: &str) -> Result<f64> {
    let (status, body) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    anyhow::ensure!(status == 200, "GET /metrics: {status}");
    let line = body.lines().find(|l| l.starts_with(series)).ok_or_else(|| anyhow::anyhow!("no {series} in:\n{body}"))?;
    Ok(line.rsplit(' ').next().unwrap_or_default().parse()?)
}

const REQUESTS: &str = r#"keel_requests_total{pool="web",vhost="*",status="200"}"#;

fn get(stack: &Stack, n: usize) -> Result<()> {
    let addr = stack.addr("keel", 80)?;
    for _ in 0..n {
        assert_eq!(http_get(addr, "e2e.test", "/")?.0, 200);
    }
    Ok(())
}

/// Wait until the request counter reads `want`: a request is counted just
/// after its response is sent.
fn requests_reach(stack: &Stack, want: f64) -> Result<()> {
    wait_until(&format!("{REQUESTS} = {want}"), Duration::from_secs(10), || {
        Ok((sample(stack, REQUESTS)? == want).then_some(()))
    })
}

#[test]
#[ignore = "needs Docker"]
fn one_endpoint_counts_every_worker_and_outlives_a_killed_one() -> Result<()> {
    let stack = Stack::up("metrics", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", CONFIG.to_owned())])?;
    let addr = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    let start = wait_until("the first request counted", Duration::from_secs(10), || Ok(sample(&stack, REQUESTS).ok()))?;

    // Both workers accept, and the endpoint sums them.
    get(&stack, 40)?;
    requests_reach(&stack, start + 40.0)?;
    let logs = stack.logs("keel")?;
    assert!(!logs.contains("metrics: failed to bind"), "a worker tried to bind the metrics port");
    assert_eq!(sample(&stack, "keel_workers ")?, 2.0);
    assert_eq!(sample(&stack, "keel_workers_configured ")?, 2.0);

    // A killed worker's counts stay; its replacement counts on top.
    let victim = logs
        .lines()
        .filter(|l| l.contains("master: worker started"))
        .find(|l| log_field(l, "index") == Some("1"))
        .and_then(|l| log_field(l, "pid"))
        .ok_or_else(|| anyhow::anyhow!("no start line for worker 1"))?
        .to_owned();
    let out = stack.exec("keel", &["sh", "-c", &format!("kill -9 {victim}")])?;
    assert!(out.status.success(), "kill failed: {}", String::from_utf8_lossy(&out.stderr));
    wait_until("worker 1 replaced", Duration::from_secs(20), || {
        Ok(stack.logs("keel")?.lines().any(|l| l.contains("master: replacement worker started")).then_some(()))
    })?;
    assert_eq!(sample(&stack, REQUESTS)?, start + 40.0, "the killed worker's requests are still counted");
    assert_eq!(sample(&stack, r#"keel_worker_restarts_total{reason="signal"}"#)?, 1.0);

    wait_until("HTTP 200 after the restart", Duration::from_secs(20), || {
        Ok(http_get(addr, "e2e.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    get(&stack, 10)?;
    requests_reach(&stack, start + 51.0)?;
    assert_eq!(sample(&stack, r#"keel_active_connections{pool="web",backend="172.29.87.20:80"}"#)?, 0.0);
    Ok(())
}

// `spare` has nothing listening: its check fails.
const POOLS_CONFIG: &str = r#"
listener "http" "0.0.0.0:80" {}

pool "web" {
  backend "172.29.87.20:80" {}
}

pool "spare" {
  health_check "tcp" {
    interval            = "1s"
    unhealthy_threshold = 1
  }

  backend "172.29.87.20:81" {}
}

vhost "*" {
  pool = "web"
}
"#;

fn pool_backends(stack: &Stack, pool: &str, state: &str) -> Result<f64> {
    sample(stack, &format!(r#"keel_pool_backends{{pool="{pool}",state="{state}"}}"#))
}

#[test]
#[ignore = "needs Docker"]
fn backends_per_pool_by_state() -> Result<()> {
    let stack = Stack::up("pool-backends", COMPOSE, &[("node.hcl", NODE.to_owned()), ("config/keel.hcl", POOLS_CONFIG.to_owned())])?;
    wait_until("spare unhealthy", Duration::from_secs(60), || {
        Ok((pool_backends(&stack, "spare", "unhealthy").ok() == Some(1.0)).then_some(()))
    })?;
    for state in ["available", "ejected", "draining"] {
        assert_eq!(pool_backends(&stack, "spare", state)?, 0.0, "spare {state}");
    }
    assert_eq!(pool_backends(&stack, "web", "available")?, 1.0);

    // Drained with no connections open: removed at once, and no longer counted.
    let out = stack.exec("keel", &["keel", "backend", "drain", "172.29.87.20:80"])?;
    assert!(out.status.success(), "drain: {}", String::from_utf8_lossy(&out.stderr));
    wait_until("web drained", Duration::from_secs(10), || {
        let counts: Vec<f64> = ["available", "unhealthy", "ejected", "draining"]
            .iter()
            .map(|s| pool_backends(&stack, "web", s))
            .collect::<Result<_>>()?;
        Ok((counts == [0.0; 4]).then_some(()))
    })?;
    assert_eq!(sample(&stack, r#"keel_backend_drain_state{pool="web",backend="172.29.87.20:80"}"#)?, 2.0, "removed");
    Ok(())
}
